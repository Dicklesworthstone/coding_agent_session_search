//! Real pinned-parser tests: no replacement JSON parser or connector stub.

use super::*;
use crate::connectors::claude_code::ClaudeCodeConnector;
use serde_json::json;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn line(role: &str, text: &str) -> String {
    format!(
        "{}\n",
        json!({
            "type":role,
            "timestamp":"2026-11-24T12:00:00Z",
            "sessionId":"session-identity",
            "cwd":"/workspace/project",
            "message":{"role":role,"content":text}
        })
    )
}

fn fixture(bytes: &str) -> Result<(tempfile::TempDir, PathBuf, ScanContext)> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("projects");
    fs::create_dir_all(root.join("project-a"))?;
    let path = root.join("project-a/session.jsonl");
    fs::write(&path, bytes)?;
    let ctx = ScanContext::with_roots(
        directory.path().join("archive"),
        vec![ScanRoot::local(root)],
        None,
    );
    Ok((directory, path, ctx))
}

fn scan_one(
    ctx: &ScanContext,
    previous: Option<&StablePrefixReceipt>,
) -> Result<(NormalizedConversation, StablePrefixReceipt)> {
    let observation = previous.map(|receipt| json!({"claude_prefix":receipt}).to_string());
    let mut before = |source: &DiscoveredSourceFile| {
        stable_prefix::set_previous_prefix_receipt_for(
            PROVIDER,
            &source.source_path,
            observation.as_deref(),
        )
        .unwrap();
        true
    };
    let mut receipts = Vec::new();
    let mut complete = |done: &SourceCompletion| {
        let receipt = stable_prefix::current_prefix_receipt(&done.source.source_path)
            .context("real parser completion has a typed receipt")?;
        assert_eq!(receipt.provider, PROVIDER);
        assert!(
            !stable_prefix::guarded_prefix_delivery_for(PROVIDER, &done.source.source_path),
            "completion cannot authorize another delivery"
        );
        if receipt.partial {
            assert_eq!(done.source.size_bytes, None);
            assert_eq!(done.source.modified_at_ms, None);
        }
        receipts.push(receipt);
        Ok(())
    };
    let mut conversations = Vec::new();
    ClaudeCodeConnector::new().scan_with_source_boundaries(
        ctx,
        &mut SourceScanHooks {
            should_scan_source: Some(&mut before),
            on_source_complete: Some(&mut complete),
        },
        &mut |conversation| {
            assert!(guarded_active_jsonl_source(&conversation.source_path));
            assert!(!stable_prefix::guarded_prefix_delivery(
                &conversation.source_path
            ));
            conversations.push(conversation);
            Ok(())
        },
    )?;
    assert_eq!(conversations.len(), 1);
    assert_eq!(receipts.len(), 1);
    Ok((conversations.remove(0), receipts.remove(0)))
}

fn contents(conversation: &NormalizedConversation) -> Vec<&str> {
    conversation
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            assert_eq!(message.idx, i64::try_from(index).unwrap());
            message.content.as_str()
        })
        .collect()
}

#[test]
fn held_writer_exposes_complete_records_and_commits_a_valid_unterminated_tail_only_after_lf()
-> Result<()> {
    let committed = line("user", "first searchable turn");
    let tail = line("assistant", "Unicode 尾巴 🔒 stays incomplete");
    let initial = committed.clone() + tail.trim_end_matches('\n');
    let (_directory, path, ctx) = fixture(&initial)?;
    let mut writer = OpenOptions::new().append(true).open(&path)?;
    assert!(can_attempt_active_jsonl_source(&path));
    let (first, receipt) = scan_one(&ctx, None)?;
    assert_eq!(contents(&first), ["first searchable turn"]);
    assert_eq!(receipt.committed_bytes, committed.len() as u64);
    assert!(receipt.partial);
    assert_eq!(fs::read_to_string(&path)?, initial);
    assert_eq!(
        fs::read(receipt.capture_view()?.path())?,
        committed.as_bytes()
    );

    writer.write_all(b"\n")?;
    let later = line("user", "next committed turn");
    writer.write_all(later.as_bytes())?;
    writer.flush()?;
    let (second, next) = scan_one(&ctx, Some(&receipt))?;
    assert_eq!(
        contents(&second),
        [
            "first searchable turn",
            "Unicode 尾巴 🔒 stays incomplete",
            "next committed turn"
        ]
    );
    assert_eq!(first.external_id, second.external_id);
    assert_eq!(next.message_count, 3);
    assert!(next.partial, "an idle open writer cannot certify full EOF");
    let (_, replay) = scan_one(&ctx, Some(&next))?;
    assert_eq!(replay.messages_blake3, next.messages_blake3);
    assert_eq!(replay.committed_bytes, next.committed_bytes);
    assert_eq!(fs::read_to_string(&path)?, initial + "\n" + &later);
    Ok(())
}

#[test]
fn inactive_legacy_eof_still_delivers_a_complete_unterminated_record() -> Result<()> {
    let record = line("user", "legacy complete JSON without LF");
    let bytes = record.trim_end_matches('\n');
    let (_directory, path, ctx) = fixture(bytes)?;
    File::open(&path)?.set_modified(
        std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60),
    )?;
    assert!(crate::indexer::source_can_recover_incomplete_tail(&path));
    let (conversation, receipt) = scan_one(&ctx, None)?;
    assert_eq!(contents(&conversation), ["legacy complete JSON without LF"]);
    assert!(receipt.inactive_eof);
    assert!(!receipt.partial);
    assert_eq!(receipt.committed_bytes, bytes.len() as u64);
    assert_eq!(fs::read(receipt.capture_view()?.path())?, bytes.as_bytes());
    Ok(())
}

#[test]
fn raising_a_source_budget_resumes_the_same_published_prefix_after_growth() -> Result<()> {
    let (_directory, path, ctx) = fixture(&line("user", "first prefix"))?;
    let low = parse_source_read_budget("512")?;
    let raised = parse_source_read_budget("4096")?;
    let parser = franken_agent_detection::connectors::claude_code::ClaudeCodeConnector::new();
    let attempt = |limit, previous: Option<&StablePrefixReceipt>| {
        let observation = previous.map(|receipt| json!({"claude_prefix":receipt}).to_string());
        let mut before = |source: &DiscoveredSourceFile| {
            stable_prefix::set_previous_prefix_receipt_for(
                PROVIDER,
                &source.source_path,
                observation.as_deref(),
            )
            .unwrap();
            true
        };
        let mut receipts = Vec::new();
        let mut complete = |done: &SourceCompletion| {
            receipts.push(stable_prefix::current_prefix_receipt(&done.source.source_path).unwrap());
            Ok(())
        };
        let mut delivered = Vec::new();
        let result = scan_with_limit(
            &parser,
            &ctx,
            &mut SourceScanHooks {
                should_scan_source: Some(&mut before),
                on_source_complete: Some(&mut complete),
            },
            &mut |conversation| {
                delivered.push(conversation);
                Ok(())
            },
            limit,
        );
        (result, delivered, receipts)
    };
    let (result, first, receipts) = attempt(low, None);
    result?;
    assert_eq!(contents(&first[0]), ["first prefix"]);
    let previous = &receipts[0];
    OpenOptions::new()
        .append(true)
        .open(&path)?
        .write_all(line("assistant", &"x".repeat(1024)).as_bytes())?;
    assert!(fs::metadata(&path)?.len() > low);
    assert!(!stable_prefix::can_attempt_for(PROVIDER, &path, low));
    let (result, delivered, completed) = attempt(low, Some(previous));
    let error = result.expect_err("existing prefix must not fall back to unbounded parsing");
    assert!(delivered.is_empty());
    assert!(completed.is_empty());
    let mut reasons = Vec::new();
    assert!(crate::connectors::codex::for_each_source_failure(
        &error,
        |_, reason| reasons.push(reason)
    ));
    assert_eq!(
        reasons,
        [crate::connectors::codex::SourceFailureReason::ReadBudgetExceeded]
    );
    assert!(stable_prefix::can_attempt_for(PROVIDER, &path, raised));
    let (result, delivered, completed) = attempt(raised, Some(previous));
    result?;
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].messages.len(), 2);
    assert_eq!(delivered[0].messages[0].content, "first prefix");
    assert_eq!(
        stable_prefix::messages_digest(&delivered[0].messages[..previous.message_count])?,
        previous.messages_blake3
    );
    assert_eq!(completed[0].message_count, 2);
    Ok(())
}

#[test]
fn source_byte_budget_configuration_rejects_unbounded_or_malformed_values() {
    for (value, expected) in [
        ("1", 1),
        (" 512 ", 512),
        ("1073741824", MAX_CONFIGURED_SOURCE_BYTES),
    ] {
        assert_eq!(parse_source_read_budget(value).unwrap(), expected);
    }
    for value in [
        "",
        " ",
        "0",
        "-1",
        "+1",
        "1.5",
        "100MiB",
        "1073741825",
        "18446744073709551616",
    ] {
        let error = parse_source_read_budget(value).unwrap_err().to_string();
        assert!(error.contains(SOURCE_LIMIT_ENV));
        assert!(error.contains("1 through 1073741824"));
    }
}

#[test]
fn captured_view_matches_real_parser_identity_for_directory_file_subagent_and_remote_roots()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let projects = directory.path().join("projects");
    let project = projects.join("project-a");
    let parent = project.join("parent-session");
    fs::create_dir_all(parent.join("subagents"))?;
    let ordinary = project.join("session.jsonl");
    let subagent = parent.join("subagents/agent-same-name.jsonl");
    let text = line("user", "identity and workspace")
        + &format!(
            "{}\n",
            json!({
                "type":"assistant", "isSidechain":false, "parentUuid":"body-parent",
                "message":{"role":"assistant","content":"assistant output"}
            })
        );
    fs::write(&ordinary, &text)?;
    fs::write(&subagent, &text)?;
    let raw = franken_agent_detection::connectors::claude_code::ClaudeCodeConnector::new();
    for root in [
        ScanRoot::local(projects.clone()),
        ScanRoot::local(directory.path().to_path_buf()),
        ScanRoot::local(ordinary),
        ScanRoot::local(subagent),
        ScanRoot::remote(
            project,
            crate::connectors::Origin {
                kind: crate::connectors::SourceKind::Ssh,
                source_id: "remote-laptop".into(),
                host: Some("laptop".into()),
            },
            Some(crate::connectors::Platform::Windows),
        )
        .with_rewrite("/workspace", "/mapped"),
    ] {
        let ctx =
            ScanContext::with_roots(directory.path().join("archive"), vec![root.clone()], None);
        let expected = raw.scan(&ctx)?;
        let mut before = |source: &DiscoveredSourceFile| {
            assert_eq!(source.scan_root, root.path);
            assert_eq!(source.origin, root.origin);
            assert_eq!(source.platform, root.platform);
            let scoped = scoped_context(&ctx, source, Path::new("/private/parser-view.jsonl"));
            assert_eq!(
                scoped.scan_roots[0].workspace_rewrites,
                root.workspace_rewrites
            );
            true
        };
        let mut complete = |done: &SourceCompletion| {
            assert_eq!(done.source.scan_root, root.path);
            assert_eq!(done.source.origin, root.origin);
            assert_eq!(done.source.platform, root.platform);
            assert!(done.source.source_path.starts_with(directory.path()));
            Ok(())
        };
        let mut actual = Vec::new();
        ClaudeCodeConnector::new().scan_with_source_boundaries(
            &ctx,
            &mut SourceScanHooks {
                should_scan_source: Some(&mut before),
                on_source_complete: Some(&mut complete),
            },
            &mut |conversation| {
                actual.push(conversation);
                Ok(())
            },
        )?;
        assert_eq!(actual.len(), expected.len());
        for (mut actual, expected) in actual.into_iter().zip(expected) {
            let receipt = stable_prefix::metadata_receipt(&actual.metadata)?.context("receipt")?;
            assert_eq!(receipt.source_path, expected.source_path);
            actual.metadata.as_object_mut().unwrap().remove("cass");
            assert_eq!(
                serde_json::to_value(actual)?,
                serde_json::to_value(expected)?
            );
        }
    }
    Ok(())
}

#[test]
fn admitted_prefix_keeps_tool_results_and_source_order_across_append_and_replay() -> Result<()> {
    let initial = line("assistant", "start")
        + &format!(
            "{}\n",
            json!({
                "type":"user", "timestamp":"2026-11-24T11:00:00Z",
                "message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call-one","is_error":true,"content":"compiler failed"},
                    {"type":"text","text":"retry this"}
                ]}
            })
        );
    let (_directory, path, ctx) = fixture(&initial)?;
    let (first, receipt) = scan_one(&ctx, None)?;
    assert_eq!(contents(&first), ["start", "compiler failed", "retry this"]);
    assert_eq!(first.messages[1].role, "tool");
    assert_eq!(first.messages[1].extra["tool_use_id"], "call-one");
    assert_eq!(first.messages[1].extra["is_error"], true);
    OpenOptions::new()
        .append(true)
        .open(path)?
        .write_all(line("assistant", "fixed").as_bytes())?;
    let (next, _) = scan_one(&ctx, Some(&receipt))?;
    assert_eq!(
        contents(&next),
        ["start", "compiler failed", "retry this", "fixed"]
    );
    Ok(())
}

#[test]
fn append_in_consumer_preserves_boundary_and_withholds_full_source_completion() -> Result<()> {
    let initial = line("user", "admitted");
    let (_directory, path, ctx) = fixture(&initial)?;
    let later = line("assistant", "written during delivery");
    let mut completed = None;
    let mut complete = |done: &SourceCompletion| {
        completed = stable_prefix::current_prefix_receipt(&done.source.source_path);
        assert_eq!(done.source.size_bytes, None);
        Ok(())
    };
    ClaudeCodeConnector::new().scan_with_source_boundaries(
        &ctx,
        &mut SourceScanHooks {
            should_scan_source: None,
            on_source_complete: Some(&mut complete),
        },
        &mut |conversation| {
            assert_eq!(contents(&conversation), ["admitted"]);
            OpenOptions::new()
                .append(true)
                .open(&path)?
                .write_all(later.as_bytes())?;
            Ok(())
        },
    )?;
    let receipt = completed.context("prefix completion")?;
    assert!(receipt.partial);
    assert_eq!(receipt.committed_bytes, initial.len() as u64);
    let (next, _) = scan_one(&ctx, Some(&receipt))?;
    assert_eq!(contents(&next), ["admitted", "written during delivery"]);
    Ok(())
}

#[test]
fn source_rewrite_after_delivery_cannot_publish_a_completion_receipt() -> Result<()> {
    for mutation in ["rewrite", "truncate", "replace"] {
        let original = line("user", "original");
        let (directory, path, ctx) = fixture(&original)?;
        let completed = Cell::new(false);
        let mut complete = |_: &SourceCompletion| {
            completed.set(true);
            Ok(())
        };
        let error = ClaudeCodeConnector::new()
            .scan_with_source_boundaries(
                &ctx,
                &mut SourceScanHooks {
                    should_scan_source: None,
                    on_source_complete: Some(&mut complete),
                },
                &mut |conversation| {
                    assert_eq!(contents(&conversation), ["original"]);
                    match mutation {
                        "rewrite" => fs::write(&path, line("user", "modified"))?,
                        "truncate" => OpenOptions::new().write(true).open(&path)?.set_len(0)?,
                        "replace" => {
                            fs::rename(&path, directory.path().join("retained.jsonl"))?;
                            fs::write(&path, &original)?;
                        }
                        _ => unreachable!(),
                    }
                    Ok(())
                },
            )
            .expect_err("a changed source must retain retry debt");
        assert!(!completed.get(), "{mutation}");
        assert!(stable_prefix::current_prefix_receipt(&path).is_none());
        let mut failed = Vec::new();
        assert!(crate::connectors::codex::for_each_source_failure(
            &error,
            |source, reason| {
                failed.push((source.to_path_buf(), reason));
            }
        ));
        assert_eq!(
            failed,
            [(
                path,
                crate::connectors::codex::SourceFailureReason::SourceChanged
            )]
        );
    }
    Ok(())
}

#[test]
fn a_changed_prior_prefix_is_rejected_before_delivery_while_healthy_siblings_continue() -> Result<()>
{
    let original = line("user", "original");
    let (_directory, path, ctx) = fixture(&original)?;
    let (_, previous) = scan_one(&ctx, None)?;
    let sibling = path.with_file_name("z-healthy.jsonl");
    fs::write(&sibling, line("user", "healthy sibling"))?;
    let observation = json!({"claude_prefix":previous}).to_string();
    let mut before = |source: &DiscoveredSourceFile| {
        if source.source_path == path {
            stable_prefix::set_previous_prefix_receipt_for(PROVIDER, &path, Some(&observation))
                .unwrap();
            fs::write(&path, line("user", "modified")).unwrap();
        }
        true
    };
    let mut delivered = Vec::new();
    let error = ClaudeCodeConnector::new()
        .scan_with_source_boundaries(
            &ctx,
            &mut SourceScanHooks {
                should_scan_source: Some(&mut before),
                on_source_complete: None,
            },
            &mut |conversation| {
                delivered.push(conversation);
                Ok(())
            },
        )
        .expect_err("rewritten published prefix cannot be reinterpreted as append");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].source_path, sibling);
    assert_eq!(contents(&delivered[0]), ["healthy sibling"]);
    let mut failed = Vec::new();
    assert!(crate::connectors::codex::for_each_source_failure(
        &error,
        |source, _| failed.push(source.to_path_buf())
    ));
    assert_eq!(failed, [path]);
    Ok(())
}

#[test]
fn consumer_cancellation_stops_siblings_and_never_certifies_source_completion() -> Result<()> {
    let (_directory, path, ctx) = fixture(&line("user", "first"))?;
    fs::write(
        path.with_file_name("z-next.jsonl"),
        line("user", "must not deliver"),
    )?;
    let delivered = Cell::new(0);
    let completed = Cell::new(0);
    let mut complete = |_: &SourceCompletion| {
        completed.set(completed.get() + 1);
        Ok(())
    };
    let error = ClaudeCodeConnector::new()
        .scan_with_source_boundaries(
            &ctx,
            &mut SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut complete),
            },
            &mut |_| {
                delivered.set(delivered.get() + 1);
                Err(io::Error::new(io::ErrorKind::Interrupted, "consumer cancellation").into())
            },
        )
        .expect_err("consumer cancellation must abort the scan");
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
    assert_eq!(delivered.get(), 1);
    assert_eq!(completed.get(), 0);
    assert!(stable_prefix::current_prefix_receipt(&path).is_none());
    Ok(())
}

#[test]
fn host_veto_locks_and_incomplete_first_record_never_gain_prefix_authority() -> Result<()> {
    let (_directory, path, ctx) = fixture(&line("user", "private"))?;
    let mut veto = |_: &DiscoveredSourceFile| false;
    let mut unexpected =
        |_: NormalizedConversation| -> Result<()> { panic!("host rejected this source") };
    ClaudeCodeConnector::new().scan_with_source_boundaries(
        &ctx,
        &mut SourceScanHooks {
            should_scan_source: Some(&mut veto),
            on_source_complete: None,
        },
        &mut unexpected,
    )?;
    let lock = File::open(&path)?;
    lock.try_lock()?;
    assert!(!can_attempt_active_jsonl_source(&path));
    assert!(ClaudeCodeConnector::new().scan(&ctx).is_err());
    lock.unlock()?;
    fs::write(
        &path,
        line("user", "valid but not committed").trim_end_matches('\n'),
    )?;
    assert!(!can_attempt_active_jsonl_source(&path));
    assert!(ClaudeCodeConnector::new().scan(&ctx).is_err());
    assert!(stable_prefix::current_prefix_receipt(&path).is_none());
    Ok(())
}

#[test]
fn modifying_source_during_capture_fails_before_any_conversation_is_delivered() -> Result<()> {
    let initial = line("user", "original");
    let (_directory, path, mut ctx) = fixture(&initial)?;
    let armed = Arc::new(AtomicBool::new(false));
    let armed_for_tick = armed.clone();
    let changed = Arc::new(AtomicBool::new(false));
    let changed_for_tick = changed.clone();
    let target = path.clone();
    ctx.progress_tick = Some(Arc::new(move || {
        if armed_for_tick.load(Ordering::Relaxed) && !changed_for_tick.swap(true, Ordering::Relaxed)
        {
            fs::write(&target, line("user", "modified")).unwrap();
        }
    }));
    let mut before = |_: &DiscoveredSourceFile| {
        armed.store(true, Ordering::Relaxed);
        true
    };
    let delivered = Cell::new(false);
    let error = ClaudeCodeConnector::new().scan_with_source_boundaries(
        &ctx,
        &mut SourceScanHooks {
            should_scan_source: Some(&mut before),
            on_source_complete: None,
        },
        &mut |_| {
            delivered.set(true);
            Ok(())
        },
    );
    assert!(error.is_err());
    assert!(changed.load(Ordering::Relaxed));
    assert!(!delivered.get());
    Ok(())
}
