//! Exercise the real pinned Pi parser and production source admission policy.

use super::super::codex::stable_prefix::StablePrefixReceipt;
use super::*;
use serde_json::json;
use std::fs::OpenOptions;
use std::io::Write;

const PROVIDERS: [StablePrefixProvider; 2] =
    [StablePrefixProvider::PiAgent, StablePrefixProvider::Omp];

fn connector(provider: StablePrefixProvider) -> Box<dyn Connector> {
    match provider {
        StablePrefixProvider::PiAgent => Box::new(super::super::pi_agent::PiAgentConnector::new()),
        StablePrefixProvider::Omp => Box::new(OmpConnector::new()),
        _ => unreachable!("Pi-family fixture"),
    }
}

fn line(id: &str, parent: Option<&str>, text: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"message", "id":id, "parentId":parent,
            "timestamp":"2026-11-24T12:00:01Z",
            "message":{"role":"user", "content":text}})
    )
}

fn header() -> String {
    format!(
        "{}\n",
        json!({"type":"session", "id":"native-session-id", "version":3,
            "timestamp":"2026-11-24T12:00:00Z", "cwd":"/project"})
    )
}

fn fixture(
    provider: StablePrefixProvider,
    bytes: &str,
) -> Result<(tempfile::TempDir, PathBuf, ScanContext)> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(match provider {
        StablePrefixProvider::PiAgent => ".pi/agent",
        StablePrefixProvider::Omp => ".omp/profiles/work/agent",
        _ => unreachable!("Pi-family fixture"),
    });
    let path = root.join("sessions/project/2026-11-24_fixture.jsonl");
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, bytes)?;
    let ctx = ScanContext::with_roots(
        directory.path().join("cass-state"),
        vec![ScanRoot::local(root)],
        None,
    );
    Ok((directory, path, ctx))
}

fn scan_one(
    provider: StablePrefixProvider,
    ctx: &ScanContext,
    previous: Option<&StablePrefixReceipt>,
) -> Result<(NormalizedConversation, StablePrefixReceipt)> {
    let observation =
        previous.map(|receipt| json!({provider.observation_key():receipt}).to_string());
    let mut before = |source: &DiscoveredSourceFile| {
        stable_prefix::set_previous_prefix_receipt_for(
            provider,
            &source.source_path,
            observation.as_deref(),
        )
        .unwrap();
        true
    };
    let mut completed = Vec::new();
    let mut complete = |done: &SourceCompletion| {
        let receipt = stable_prefix::current_prefix_receipt(&done.source.source_path)
            .context("prefix completion retains the original source receipt")?;
        assert_eq!(receipt.provider, provider);
        assert!(!guarded_active_jsonl_source(&done.source.source_path));
        if receipt.partial {
            assert_eq!(done.source.size_bytes, None);
            assert_eq!(done.source.modified_at_ms, None);
        }
        assert!(done.required_sidecars.is_empty());
        completed.push(receipt);
        Ok(())
    };
    let mut conversations = Vec::new();
    connector(provider).scan_with_source_boundaries(
        ctx,
        &mut SourceScanHooks {
            should_scan_source: Some(&mut before),
            on_source_complete: Some(&mut complete),
        },
        &mut |conversation| {
            assert!(guarded_active_jsonl_source(&conversation.source_path));
            assert!(stable_prefix::guarded_prefix_delivery_for(
                provider,
                &conversation.source_path
            ));
            assert!(!stable_prefix::guarded_prefix_delivery(
                &conversation.source_path
            ));
            conversations.push(conversation);
            Ok(())
        },
    )?;
    assert_eq!(conversations.len(), 1);
    assert_eq!(completed.len(), 1);
    Ok((conversations.remove(0), completed.remove(0)))
}

fn contents(conversation: &NormalizedConversation) -> Vec<&str> {
    conversation
        .messages
        .iter()
        .enumerate()
        .map(|(idx, message)| {
            assert_eq!(message.idx, i64::try_from(idx).unwrap());
            message.content.as_str()
        })
        .collect()
}

#[test]
fn held_writer_preserves_branches_identity_and_profiles_until_tail_is_complete() -> Result<()> {
    for provider in PROVIDERS {
        let prefix = header()
            + &line("a", None, "first searchable turn")
            + &line("b", Some("a"), "abandoned branch remains searchable")
            + &line("c", Some("a"), "current branch remains searchable")
            + &format!(
                "{}\n",
                json!({"type":"compaction", "id":"d", "parentId":"c",
                "firstKeptEntryId":"c", "summary":"checkpoint summary"})
            );
        let tail = line("e", Some("d"), "Unicode 尾巴 🔒 stays incomplete");
        let initial = prefix.clone() + tail.trim_end_matches('\n');
        let (_directory, path, ctx) = fixture(provider, &initial)?;
        let mut writer = OpenOptions::new().append(true).open(&path)?;
        let before = fs::metadata(&path)?.modified()?;
        assert!(can_attempt_active_jsonl_source(provider.slug(), &path));
        let (first, receipt) = scan_one(provider, &ctx, None)?;
        assert_eq!(
            contents(&first),
            [
                "first searchable turn",
                "abandoned branch remains searchable",
                "current branch remains searchable",
                "[compaction] checkpoint summary"
            ]
        );
        assert_eq!(first.external_id.as_deref(), Some("native-session-id"));
        assert_eq!(first.source_path, path);
        assert_eq!(
            first
                .metadata
                .get("profile")
                .and_then(|value| value.as_str()),
            (provider == StablePrefixProvider::Omp).then_some("work")
        );
        assert_eq!(
            first.messages[1].extra.pointer("/cass/on_active_branch"),
            Some(&json!(false))
        );
        assert_eq!(receipt.committed_bytes, prefix.len() as u64);
        assert!(receipt.partial);
        assert_eq!(fs::read_to_string(&path)?, initial);
        assert_eq!(fs::metadata(&path)?.modified()?, before);
        assert_eq!(fs::read(receipt.capture_view()?.path())?, prefix.as_bytes());

        writer.write_all(b"\n")?;
        writer.flush()?;
        let (next, updated) = scan_one(provider, &ctx, Some(&receipt))?;
        assert_eq!(contents(&next)[..4], contents(&first));
        assert_eq!(next.messages[4].content, "Unicode 尾巴 🔒 stays incomplete");
        assert_eq!(first.external_id, next.external_id);
        assert_eq!(updated.message_count, 5);
        assert!(updated.partial, "a held writer cannot certify complete EOF");
        let (replay, replay_receipt) = scan_one(provider, &ctx, Some(&updated))?;
        assert_eq!(contents(&replay), contents(&next));
        assert_eq!(replay_receipt.messages_blake3, updated.messages_blake3);
        let direct = franken_agent_detection::connectors::pi_wire::parse_session_file(
            &path,
            path.parent().unwrap(),
            provider.slug(),
        )
        .context("the same pinned parser can read the complete fixture")?;
        assert_eq!(
            serde_json::to_value(&next.messages)?,
            serde_json::to_value(&direct.messages)?
        );
    }
    Ok(())
}

#[test]
fn rewritten_prior_prefix_is_refused_while_healthy_siblings_continue() -> Result<()> {
    for provider in PROVIDERS {
        for mutation in ["rewrite", "truncate", "replace"] {
            let original = header() + &line("a", None, "original");
            let (directory, path, ctx) = fixture(provider, &original)?;
            let (_, previous) = scan_one(provider, &ctx, None)?;
            let sibling = path.with_file_name("2026-11-24_z-healthy.jsonl");
            fs::write(&sibling, header() + &line("z", None, "healthy sibling"))?;
            let observation = json!({provider.observation_key():previous}).to_string();
            let mut before = |source: &DiscoveredSourceFile| {
                if source.source_path == path {
                    stable_prefix::set_previous_prefix_receipt_for(
                        provider,
                        &path,
                        Some(&observation),
                    )
                    .unwrap();
                    match mutation {
                        "rewrite" => {
                            fs::write(&path, header() + &line("a", None, "modified")).unwrap()
                        }
                        "truncate" => OpenOptions::new()
                            .write(true)
                            .open(&path)
                            .unwrap()
                            .set_len(0)
                            .unwrap(),
                        "replace" => {
                            fs::rename(&path, directory.path().join("retained.jsonl")).unwrap();
                            fs::write(&path, &original).unwrap();
                        }
                        _ => unreachable!(),
                    }
                }
                true
            };
            let mut delivered = Vec::new();
            let mut completed = Vec::new();
            let error = connector(provider)
                .scan_with_source_boundaries(
                    &ctx,
                    &mut SourceScanHooks {
                        should_scan_source: Some(&mut before),
                        on_source_complete: Some(&mut |done| {
                            completed.push(done.source.source_path.clone());
                            Ok(())
                        }),
                    },
                    &mut |conversation| {
                        delivered.push(conversation);
                        Ok(())
                    },
                )
                .expect_err("changed published bytes must retain retry debt");
            assert_eq!(delivered.len(), 1);
            assert_eq!(delivered[0].source_path, sibling);
            assert_eq!(contents(&delivered[0]), ["healthy sibling"]);
            assert_eq!(completed, [sibling]);
            let mut failed = Vec::new();
            assert!(super::super::codex::for_each_source_failure(
                &error,
                |source, _| failed.push(source.to_path_buf())
            ));
            assert_eq!(failed, [path]);
        }
    }
    Ok(())
}

#[test]
fn mutation_during_delivery_never_certifies_source_completion() -> Result<()> {
    for provider in PROVIDERS {
        let original = header() + &line("a", None, "original");
        let (_directory, path, ctx) = fixture(provider, &original)?;
        let mut completions = 0;
        let error = connector(provider)
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
                    assert_eq!(contents(&conversation), ["original"]);
                    fs::write(&path, header() + &line("a", None, "modified"))?;
                    Ok(())
                },
            )
            .expect_err("a post-delivery rewrite must not publish a source receipt");
        assert_eq!(completions, 0);
        assert!(stable_prefix::current_prefix_receipt(&path).is_none());
        assert!(super::super::codex::for_each_source_failure(
            &error,
            |source, _| assert_eq!(source, path)
        ));
    }
    Ok(())
}

#[test]
fn metadata_only_prefix_retains_retry_debt_until_inactive_eof_can_be_read() -> Result<()> {
    for provider in PROVIDERS {
        let tail = line("a", None, "waiting for newline");
        let (_directory, path, ctx) = fixture(provider, &(header() + tail.trim_end_matches('\n')))?;
        let error = connector(provider)
            .scan_with_source_boundaries(
                &ctx,
                &mut SourceScanHooks {
                    should_scan_source: None,
                    on_source_complete: Some(&mut |_| {
                        anyhow::bail!("no message cannot certify completion")
                    }),
                },
                &mut |_| anyhow::bail!("incomplete first message was delivered"),
            )
            .expect_err("the withheld first message must keep watch retry debt");
        let mut reasons = Vec::new();
        assert!(super::super::codex::for_each_source_failure(
            &error,
            |source, reason| {
                assert_eq!(source, path);
                reasons.push(reason);
            }
        ));
        assert_eq!(
            reasons,
            [super::super::codex::SourceFailureReason::UnfinishedSource]
        );
        let bytes = fs::read(&path)?;
        // Model the retry after the writer closed and the inactivity window
        // elapsed without another write or filesystem content notification.
        fs::File::open(&path)?.set_modified(
            std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60),
        )?;
        let (conversation, receipt) = scan_one(provider, &ctx, None)?;
        assert_eq!(contents(&conversation), ["waiting for newline"]);
        assert!(!receipt.partial);
        assert!(receipt.inactive_eof);
        assert_eq!(fs::read(&path)?, bytes);
    }
    Ok(())
}

#[test]
fn exclusions_precede_private_prefix_capture_and_source_callbacks() -> Result<()> {
    const CHILD_ROOT: &str = "CASS_PI_PREFIX_EXCLUSION_ROOT";
    const CHILD_PROVIDER: &str = "CASS_PI_PREFIX_EXCLUSION_PROVIDER";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let provider = StablePrefixProvider::from_slug(&std::env::var(CHILD_PROVIDER)?).unwrap();
        let ctx = ScanContext::with_roots(
            PathBuf::from(&root).join("cass-state"),
            vec![ScanRoot::local(root.into())],
            None,
        );
        let mut admissions = Vec::new();
        let mut completions = Vec::new();
        let mut delivered = Vec::new();
        connector(provider).scan_with_source_boundaries(
            &ctx,
            &mut SourceScanHooks {
                should_scan_source: Some(&mut |source| {
                    admissions.push(source.source_path.clone());
                    true
                }),
                on_source_complete: Some(&mut |done| {
                    completions.push(done.source.source_path.clone());
                    Ok(())
                }),
            },
            &mut |conversation| {
                delivered.push(conversation);
                Ok(())
            },
        )?;
        assert_eq!(delivered.len(), 1);
        assert_eq!(contents(&delivered[0]), ["public sibling"]);
        assert_eq!(admissions, [delivered[0].source_path.clone()]);
        assert_eq!(completions, admissions);
        return Ok(());
    }
    for provider in PROVIDERS {
        let (_directory, private, ctx) =
            fixture(provider, &(header() + &line("a", None, "excluded secret")))?;
        let public = private.with_file_name("2026-11-24_public.jsonl");
        fs::write(public, header() + &line("b", None, "public sibling"))?;
        let module = module_path!().split_once("::").unwrap().1;
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                &format!(
                    "{module}::exclusions_precede_private_prefix_capture_and_source_callbacks"
                ),
                "--nocapture",
            ])
            .env(CHILD_ROOT, &ctx.scan_roots[0].path)
            .env(CHILD_PROVIDER, provider.slug())
            .env("CASS_EXCLUDE_PATHS", &private)
            .output()?;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "child test did not run"
        );
    }
    Ok(())
}
