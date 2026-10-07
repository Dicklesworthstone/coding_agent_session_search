//! CASS source checkpoints for the shared Codebuff / Freebuff store.
//!
//! FAD owns discovery, normalization and the GH511 timestamp policy. Its
//! collecting scan cannot expose per-file completions, so admit each discovered
//! transcript through CASS's ledger before delegating its parsing back to FAD.

mod discovery;
mod scan_failures;
mod watch_scope;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Result;
use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};

use super::codex::path_policy::ScanExclusions;
use super::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    ScanContext, ScanRoot,
};

#[derive(Default)]
pub struct CodebuffConnector {
    inner: franken_agent_detection::connectors::codebuff::CodebuffConnector,
}

fn source_group(source: &DiscoveredSourceFile) -> (String, PathBuf) {
    (
        source.origin.source_id.clone(),
        source
            .source_path
            .parent()
            .unwrap_or(&source.source_path)
            .to_path_buf(),
    )
}

fn parent_modified(source: &DiscoveredSourceFile) -> Option<SystemTime> {
    std::fs::metadata(source.source_path.parent()?)
        .ok()?
        .modified()
        .ok()
}

// Narrowing a scan root changes its selector, not the observed file identity.
fn same_observation(left: &DiscoveredSourceFile, right: &DiscoveredSourceFile) -> bool {
    left.provider_slug == right.provider_slug
        && left.source_path == right.source_path
        && left.role == right.role
        && left.origin == right.origin
        && left.platform == right.platform
        && left.size_bytes == right.size_bytes
        && left.modified_at_ms == right.modified_at_ms
}

impl CodebuffConnector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn source_excluded(source: &DiscoveredSourceFile, exclusions: &ScanExclusions) -> bool {
        // FAD always consults the adjacent run state when reconstructing a
        // chat. Excluding either input defers the whole reconstruction unit;
        // do not read an excluded sidecar or synthesize replacement metadata.
        // Check even a currently absent sidecar so its later creation cannot
        // turn an admitted transcript into an excluded-input read.
        exclusions.excludes(&source.source_path.with_file_name("chat-messages.json"))
            || exclusions.excludes(&source.source_path.with_file_name("run-state.json"))
    }

    fn discover_allowed(
        &self,
        ctx: &ScanContext,
        exclusions: &ScanExclusions,
    ) -> Result<Vec<DiscoveredSourceFile>> {
        exclusions.validate()?;
        let mut scoped = ctx.clone();
        if !ctx.use_default_detection() {
            scoped
                .scan_roots
                .retain(|root| !exclusions.excludes(&root.path));
            if scoped.scan_roots.is_empty() {
                // Empty *selected* roots must never fall through to FAD's
                // implicit home discovery, even when that home has history.
                return Ok(Vec::new());
            }
        }
        // A watch event can name the run state or its chat directory, while
        // FAD's exact-file entry point accepts only chat-messages.json. Keep
        // this a single reconstruction unit, never a scan of the whole store.
        // Filter the requested selector before translating it, and preserve
        // its provenance/path mappings when source_context narrows it again.
        let requested_roots = scoped.scan_roots.clone();
        for root in &mut scoped.scan_roots {
            root.path = watch_scope::transcript_selector(&root.path);
        }
        // Keep FAD's layout recognition and metadata-only enumeration. Do not
        // turn a real discovery failure into a purported complete empty scan.
        let mut sources = self.inner.discover_source_files(&scoped)?;
        for source in &mut sources {
            if let Some((_, requested)) =
                scoped
                    .scan_roots
                    .iter()
                    .zip(&requested_roots)
                    .find(|(root, _)| {
                        root.path == source.scan_root
                            && root.origin == source.origin
                            && root.platform == source.platform
                    })
            {
                source.scan_root = requested.path.clone();
            }
        }
        sources.retain(|source| !Self::source_excluded(source, exclusions));
        Ok(sources)
    }

    fn source_context(ctx: &ScanContext, source: &DiscoveredSourceFile) -> ScanContext {
        let root = ctx
            .scan_roots
            .iter()
            .find(|root| root.path == source.scan_root && root.origin == source.origin)
            .map(|root| root.with_path(source.source_path.clone()))
            .unwrap_or_else(|| {
                ScanRoot::remote(
                    source.source_path.clone(),
                    source.origin.clone(),
                    source.platform,
                )
            });
        let mut single = ctx.clone();
        single.scan_roots = vec![root];
        // Admission already selected this source. Do not reapply a watermark
        // between admission and parsing (notably after repairing an old chat).
        single.since_ts = None;
        single
    }

    fn unchanged(
        &self,
        ctx: &ScanContext,
        source: &DiscoveredSourceFile,
        sidecars: &[DiscoveredSourceFile],
        directory_before: Option<SystemTime>,
    ) -> Result<bool> {
        // Also compare the sidecar SET: a run-state file that appeared after
        // discovery must not authorize completion of an earlier observation.
        // This is another exact-file discovery, never another store traversal.
        let after = self.inner.discover_source_files(ctx)?;
        let after_sidecars: Vec<_> = after
            .iter()
            .filter(|entry| entry.role == DiscoveredSourceRole::MetadataSidecar)
            .collect();
        Ok(source.size_bytes.is_some()
            && source.modified_at_ms.is_some()
            && after.iter().any(|entry| same_observation(source, entry))
            && after_sidecars.len() == sidecars.len()
            && sidecars.iter().all(|before| {
                before.size_bytes.is_some()
                    && before.modified_at_ms.is_some()
                    && after_sidecars
                        .iter()
                        .any(|after| same_observation(before, after))
                    && !before.fs_metadata_changed()
            })
            && !source.fs_metadata_changed()
            && directory_before.is_some()
            && parent_modified(source) == directory_before)
    }
}

impl Connector for CodebuffConnector {
    fn detect(&self) -> DetectionResult {
        self.inner.detect()
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let exclusions = ScanExclusions::from_env();
        discovery::discover_for_inventory(ctx, |scope| self.discover_allowed(scope, &exclusions))
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn supports_source_boundaries(&self) -> bool {
        true
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = Vec::new();
        self.scan_with_callback(ctx, &mut |conversation| {
            conversations.push(conversation);
            Ok(())
        })?;
        Ok(conversations)
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.scan_with_source_boundaries(ctx, &mut SourceScanHooks::default(), on_conversation)
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        let exclusions = ScanExclusions::from_env();
        let mut discovery = ctx.clone();
        if hooks.should_scan_source.is_some() {
            // A durable source predicate, not a global time cutoff, owns resume.
            // Otherwise an old failed source can be excluded before admission.
            discovery.since_ts = None;
        }
        let mut failures = scan_failures::ScanFailures::default();
        let sources = discovery::discover_for_scan(&discovery, &mut failures, |scope| {
            self.discover_allowed(scope, &exclusions)
        })?;
        let mut dependencies: HashMap<_, Vec<_>> = HashMap::new();
        for source in &sources {
            if source.role == DiscoveredSourceRole::MetadataSidecar {
                dependencies
                    .entry(source_group(source))
                    .or_default()
                    .push(source.clone());
            }
        }
        for source in sources {
            if source.role != DiscoveredSourceRole::PrimarySessionLog
                || Self::source_excluded(&source, &exclusions)
                || !hooks.should_scan(&source)
            {
                continue;
            }
            // Re-resolve aliases after the host's admission hook and before
            // opening any content. A prior callback can retarget a later
            // source's symlink; a discovery-time decision is not a cache.
            // This is admission enforcement, not a handle-bound sandbox.
            if Self::source_excluded(&source, &exclusions) {
                continue;
            }
            let sidecars = dependencies
                .remove(&source_group(&source))
                .unwrap_or_default();
            let single = Self::source_context(ctx, &source);
            let directory_before = parent_modified(&source);
            let mut conversations_emitted = 0usize;
            let mut delivery_failed = false;
            let mut metadata_unavailable = false;
            let result = self
                .inner
                .scan_with_callback(&single, &mut |conversation| {
                    // FAD retains chat text when optional run state cannot be
                    // read. Do not freeze that degraded result into a durable
                    // completion: permissions can be repaired without changing
                    // the sidecar's size or modification time.
                    metadata_unavailable |=
                        conversation.metadata["run_state_status"] == "unreadable_or_oversized";
                    let result = on_conversation(conversation);
                    delivery_failed = result.is_err();
                    result?;
                    conversations_emitted += 1;
                    Ok(())
                })
                .and_then(|()| {
                    if hooks.on_source_complete.is_none() {
                        // Plain streaming/collecting callers do not certify
                        // source reuse and need no post-parse discovery.
                        return Ok(false);
                    }
                    if Self::source_excluded(&source, &exclusions) {
                        return Ok(false);
                    }
                    self.unchanged(&single, &source, &sidecars, directory_before)
                });
            match result {
                Err(error) if delivery_failed => return Err(error),
                Err(error) => {
                    failures.record(source.source_path.clone(), error);
                }
                Ok(false) => {
                    // Messages may have been emitted, but changed/unobservable
                    // inputs must remain eligible for a subsequent scan.
                }
                Ok(true) if metadata_unavailable => {}
                Ok(true) => hooks.complete(&SourceCompletion {
                    source,
                    // Optional to parse, but consulted metadata is a dependency
                    // of reuse. CASS's conservative parent policy also protects
                    // creation/removal of an initially absent run-state file.
                    required_sidecars: sidecars,
                    conversations_emitted,
                })?,
            }
        }
        failures.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, UNIX_EPOCH};

    fn fixture(root: &Path, project: &str) -> PathBuf {
        let chat = root.join(project).join("chats/2026-03-21T17-14-03.768Z");
        fs::create_dir_all(&chat).unwrap();
        let transcript = chat.join("chat-messages.json");
        fs::write(
            &transcript,
            serde_json::to_vec(&json!([
                {"id":"user-1774113351457", "variant":"user",
                 "content":"Synthetic probe question", "timestamp":"01:15 PM"},
                {"id":"ai-1774113411457", "variant":"ai",
                 "content":"Synthetic probe answer", "timestamp":"01:16 PM"}
            ]))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            chat.join("run-state.json"),
            br#"{"sessionState":{"fileContext":{"projectRoot":"/synthetic/probe"}}}"#,
        )
        .unwrap();
        transcript
    }

    fn context(root: &Path) -> ScanContext {
        ScanContext::with_roots(
            root.join("cass-data"),
            vec![ScanRoot::local(root.to_path_buf())],
            None,
        )
    }

    fn collect(
        connector: &dyn Connector,
        ctx: &ScanContext,
    ) -> (
        Vec<NormalizedConversation>,
        Vec<SourceCompletion>,
        Result<()>,
    ) {
        let mut conversations = Vec::new();
        let mut completions = Vec::new();
        let result = connector.scan_with_source_boundaries(
            ctx,
            &mut SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut |completion| {
                    completions.push(completion.clone());
                    Ok(())
                }),
            },
            &mut |conversation| {
                conversations.push(conversation);
                Ok(())
            },
        );
        (conversations, completions, result)
    }

    #[test]
    fn gh511_runtime_factory_checkpoints_fad_output_and_sidecar_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        let path = fixture(&root, "probe");
        let before = fs::read(&path).unwrap();
        let ctx = context(&root);
        let factory = super::super::get_connector_factories()
            .into_iter()
            .find(|(slug, _)| *slug == "codebuff")
            .unwrap()
            .1;
        let connector = factory();
        assert!(connector.supports_source_boundaries());
        assert_eq!(
            super::super::source_dependencies::source_dependency_policy("codebuff"),
            super::super::source_dependencies::SourceDependencyPolicy::ObserveParentDirectory
        );
        let discovered = connector.discover_source_files(&ctx).unwrap();
        let (conversations, completions, result) = collect(connector.as_ref(), &ctx);
        result.unwrap();
        let upstream = franken_agent_detection::connectors::codebuff::CodebuffConnector::new()
            .scan(&ctx)
            .unwrap();
        assert_eq!(
            serde_json::to_value(&conversations).unwrap(),
            serde_json::to_value(&upstream).unwrap()
        );
        assert_eq!(conversations.len(), 1);
        assert_eq!(
            conversations[0].messages[0].created_at,
            Some(1_774_113_351_457)
        );
        assert_eq!(
            conversations[0].messages[1].created_at,
            Some(1_774_113_411_457)
        );
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].conversations_emitted, 1);
        assert_eq!(completions[0].source, discovered[0]);
        assert_eq!(completions[0].source.scan_root, root);
        assert_eq!(
            completions[0].required_sidecars,
            vec![discovered[1].clone()]
        );
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn gh511_boundary_skip_prevents_parsing_a_bad_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        let bad = fixture(&root, "a-bad");
        let good = fixture(&root, "z-good");
        fs::write(&bad, b"[").unwrap();
        let connector = CodebuffConnector::new();
        let mut admissions = Vec::new();
        let mut completions = Vec::new();
        let mut conversations = Vec::new();
        connector
            .scan_with_source_boundaries(
                &context(&root),
                &mut SourceScanHooks {
                    should_scan_source: Some(&mut |source| {
                        admissions.push(source.source_path.clone());
                        source.source_path != bad
                    }),
                    on_source_complete: Some(&mut |completion| {
                        completions.push(completion.clone());
                        Ok(())
                    }),
                },
                &mut |conversation| {
                    conversations.push(conversation);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(admissions, vec![bad.clone(), good.clone()]);
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].source_path, good);
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].source.source_path, good);
        assert!(connector.scan(&context(&root)).is_err());
        assert_eq!(fs::read(bad).unwrap(), b"[");
    }

    #[test]
    fn gh511_boundaries_keep_good_chats_on_either_side_of_a_parse_failure() {
        for bad_project in ["a-bad", "z-bad"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("projects");
            let bad = fixture(&root, bad_project);
            let good = fixture(&root, "middle-good");
            fs::write(&bad, b"[").unwrap();
            let (conversations, completions, result) =
                collect(&CodebuffConnector::new(), &context(&root));
            let error = result.unwrap_err();
            assert!(error.to_string().contains(&bad.display().to_string()));
            assert!(error.chain().any(|cause| cause.is::<serde_json::Error>()));
            assert_eq!(conversations.len(), 1);
            assert_eq!(conversations[0].source_path, good);
            assert_eq!(completions.len(), 1);
            assert_eq!(completions[0].source.source_path, good);
        }
    }

    #[test]
    fn gh511_sink_and_completion_failures_abort_without_certifying_more_sources() {
        for fail_completion in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("projects");
            fixture(&root, "a-first");
            fixture(&root, "z-second");
            let mut delivered = 0;
            let mut completions = 0;
            let error = CodebuffConnector::new()
                .scan_with_source_boundaries(
                    &context(&root),
                    &mut SourceScanHooks {
                        should_scan_source: None,
                        on_source_complete: Some(&mut |_| {
                            completions += 1;
                            Err(std::io::Error::other("completion sink failed").into())
                        }),
                    },
                    &mut |_| {
                        delivered += 1;
                        if fail_completion {
                            Ok(())
                        } else {
                            Err(std::io::Error::other("conversation sink failed").into())
                        }
                    },
                )
                .unwrap_err();
            assert_eq!(delivered, 1, "no later source may run after a sink failure");
            assert_eq!(completions, usize::from(fail_completion));
            assert!(error.chain().any(|cause| cause.is::<std::io::Error>()));
            assert!(error.to_string().contains("sink failed"));
        }
    }

    #[test]
    fn gh511_changed_primary_or_sidecar_never_completes() {
        for target in ["chat-messages.json", "run-state.json"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("projects");
            let primary = fixture(&root, "probe");
            let mut completions = 0;
            let mut delivered = 0;
            CodebuffConnector::new()
                .scan_with_source_boundaries(
                    &context(&root),
                    &mut SourceScanHooks {
                        should_scan_source: None,
                        on_source_complete: Some(&mut |_| {
                            completions += 1;
                            Ok(())
                        }),
                    },
                    &mut |_| {
                        delivered += 1;
                        fs::write(primary.with_file_name(target), b"changed during delivery")?;
                        Ok(())
                    },
                )
                .unwrap();
            assert_eq!(delivered, 1);
            assert_eq!(completions, 0, "changed {target} must remain retryable");
        }
    }

    #[test]
    fn gh511_new_optional_sidecar_during_delivery_prevents_completion() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        let primary = fixture(&root, "probe");
        let state = primary.with_file_name("run-state.json");
        // Preserve the original fixture rather than deleting it.
        fs::rename(&state, primary.with_file_name("saved-run-state.json")).unwrap();
        let mut completions = 0;
        CodebuffConnector::new()
            .scan_with_source_boundaries(
                &context(&root),
                &mut SourceScanHooks {
                    should_scan_source: None,
                    on_source_complete: Some(&mut |_| {
                        completions += 1;
                        Ok(())
                    }),
                },
                &mut |conversation| {
                    assert!(conversation.workspace.is_none());
                    fs::write(
                        &state,
                        br#"{"sessionState":{"fileContext":{"projectRoot":"/late/workspace"}}}"#,
                    )?;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(completions, 0);
        let (conversations, completions, result) =
            collect(&CodebuffConnector::new(), &context(&root));
        result.unwrap();
        assert_eq!(
            conversations[0].workspace.as_deref(),
            Some(Path::new("/late/workspace"))
        );
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].required_sidecars[0].source_path, state);
    }

    #[test]
    fn gh511_source_resume_recovers_old_failure_without_reemitting_completed_chat() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        let bad = fixture(&root, "a-bad");
        let good = fixture(&root, "z-good");
        let original = fs::read(&bad).unwrap();
        fs::write(&bad, b"[").unwrap();
        let connector = CodebuffConnector::new();
        let (_, completions, result) = collect(&connector, &context(&root));
        assert!(result.is_err());
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].source.source_path, good);
        fs::write(&bad, original).unwrap();
        for path in [&bad, &bad.with_file_name("run-state.json")] {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(1_774_113_351))
                .unwrap();
        }
        let mut ctx = context(&root);
        ctx.since_ts = Some(4_102_444_800_000);
        assert!(connector.discover_source_files(&ctx).unwrap().is_empty());
        let mut resumed = Vec::new();
        let mut completed = Vec::new();
        connector
            .scan_with_source_boundaries(
                &ctx,
                &mut SourceScanHooks {
                    should_scan_source: Some(&mut |source| {
                        !completions.iter().any(|done| done.source == *source)
                    }),
                    on_source_complete: Some(&mut |completion| {
                        completed.push(completion.source.source_path.clone());
                        Ok(())
                    }),
                },
                &mut |conversation| {
                    resumed.push(conversation.source_path);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(resumed, vec![bad.clone()]);
        assert_eq!(completed, vec![bad]);
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn gh511_unreadable_optional_metadata_is_not_certified_until_repaired() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        let chat = root.join("probe/chats/2026-03-21T17-14-03.768Z");
        fs::create_dir_all(&chat).unwrap();
        fs::write(
            chat.join("chat-messages.json"),
            serde_json::to_vec(&json!([
                {"id":"user-1774113351457", "variant":"user",
                 "content":"metadata repair", "timestamp":"01:15 PM"}
            ]))
            .unwrap(),
        )
        .unwrap();
        let state = chat.join("run-state.json");
        let valid = br#"{"sessionState":{"fileContext":{"projectRoot":"/synthetic/probe"}}}"#;
        let mut invalid = vec![b' '; valid.len()];
        invalid[0] = b'[';
        let old = UNIX_EPOCH + Duration::from_secs(1_774_113_351);
        let connector = CodebuffConnector::new();
        let ctx =
            ScanContext::with_roots(temp.path().join("data"), vec![ScanRoot::local(root)], None);
        for (bytes, expected_completions) in [(&invalid[..], 0), (&valid[..], 1)] {
            fs::write(&state, bytes).unwrap();
            fs::File::options()
                .write(true)
                .open(&state)
                .unwrap()
                .set_modified(old)
                .unwrap();
            let mut completions = 0;
            let mut delivered = 0;
            connector
                .scan_with_source_boundaries(
                    &ctx,
                    &mut SourceScanHooks {
                        should_scan_source: None,
                        on_source_complete: Some(&mut |_| {
                            completions += 1;
                            Ok(())
                        }),
                    },
                    &mut |conversation| {
                        delivered += 1;
                        assert_eq!(conversation.workspace.is_some(), expected_completions == 1);
                        Ok(())
                    },
                )
                .unwrap();
            assert_eq!(delivered, 1, "metadata failure must not hide chat content");
            assert_eq!(completions, expected_completions);
        }
    }
}
