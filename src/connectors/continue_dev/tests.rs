use super::*;
use std::cell::Cell;
use std::fs;
use std::time::{Duration, UNIX_EPOCH};

use crate::connectors::{Origin, ScanRoot};

fn seed(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        serde_json::to_vec(&serde_json::json!({
            "sessionId": path.file_stem().unwrap().to_str().unwrap(),
            "title": "Continue admission fixture",
            "workspaceDirectory": "/remote/project",
            "history": [
                {"message": {"role": "user", "content": "preserve this question"}},
                {"message": {"role": "assistant", "content": "preserve this answer"}}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
}

fn context(root: &Path) -> ScanContext {
    ScanContext::with_roots(
        root.join("cass"),
        vec![ScanRoot::local(root.to_path_buf())],
        None,
    )
}

#[test]
fn gh552_shared_root_foreign_json_does_not_claim_continue_completion() {
    let temp = tempfile::Builder::new()
        .prefix("cass-gh552-continue-")
        .disable_cleanup(true)
        .tempdir()
        .unwrap();
    seed(&temp.path().join("native.json"));
    fs::write(
        temp.path().join("foreign.json"),
        serde_json::to_vec(&serde_json::json!({
            "id": "other-provider", "title": "Foreign snapshot", "mapping": {},
            "payload": "x".repeat(128 * 1024)
        }))
        .unwrap(),
    )
    .unwrap();
    let connector = ContinueConnector::new();
    let ctx = context(temp.path());
    assert_eq!(
        connector.discover_allowed(&ctx, &|_| false).unwrap().len(),
        2
    );
    let mut admitted = Vec::new();
    let mut completed = Vec::new();
    let mut conversations = Vec::new();
    connector
        .scan_allowed(
            &ctx,
            &mut SourceScanHooks {
                should_scan_source: Some(&mut |source| {
                    admitted.push(source.source_path.clone());
                    true
                }),
                on_source_complete: Some(&mut |completion| {
                    completed.push(completion.source.source_path.clone());
                    Ok(())
                }),
            },
            &mut |conversation| {
                conversations.push(conversation);
                Ok(())
            },
            &|_| false,
        )
        .unwrap();
    assert_eq!(
        admitted.len(),
        2,
        "both candidates receive ledger read admission"
    );
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].external_id.as_deref(), Some("native"));
    assert_eq!(
        completed,
        vec![fs::canonicalize(temp.path().join("native.json")).unwrap()]
    );
}

#[test]
fn gh552_continue_fields_and_invalid_json_still_fail_closed() {
    for payload in [
        "{invalid",
        "{}",
        r#"{"sessionId":null}"#,
        r#"{"history":null}"#,
        r#"{"workspaceDirectory":null}"#,
    ] {
        let temp = tempfile::Builder::new()
            .prefix("cass-gh552-invalid-")
            .disable_cleanup(true)
            .tempdir()
            .unwrap();
        fs::write(temp.path().join("bad.json"), payload).unwrap();
        let mut completed = 0;
        let mut delivered = 0;
        let error = ContinueConnector::new()
            .scan_allowed(
                &context(temp.path()),
                &mut SourceScanHooks {
                    should_scan_source: Some(&mut |_| true),
                    on_source_complete: Some(&mut |_| {
                        completed += 1;
                        Ok(())
                    }),
                },
                &mut |_| {
                    delivered += 1;
                    Ok(())
                },
                &|_| false,
            )
            .unwrap_err();
        assert_eq!(completed, 0);
        assert_eq!(delivered, 0);
        assert!(
            error
                .to_string()
                .contains("1 admitted snapshot(s) did not complete")
        );
        assert!(error.to_string().contains("bad.json"));
    }
}

#[test]
fn gh552_known_continue_store_does_not_disclaim_invalid_snapshot() {
    let temp = tempfile::Builder::new()
        .prefix("cass-gh552-native-")
        .disable_cleanup(true)
        .tempdir()
        .unwrap();
    let root = temp.path().join(".continue/sessions");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("bad.json"), br#"{"id":"foreign","mapping":{}}"#).unwrap();
    let mut completed = 0;
    let mut delivered = 0;
    let error = ContinueConnector::new()
        .scan_allowed(
            &context(&root),
            &mut SourceScanHooks {
                should_scan_source: Some(&mut |_| true),
                on_source_complete: Some(&mut |_| {
                    completed += 1;
                    Ok(())
                }),
            },
            &mut |_| {
                delivered += 1;
                Ok(())
            },
            &|_| false,
        )
        .unwrap_err();
    assert_eq!(completed, 0);
    assert_eq!(delivered, 0);
    assert!(
        error
            .to_string()
            .contains("1 admitted snapshot(s) did not complete")
    );
}

#[test]
fn gh552_custom_continue_store_does_not_disclaim_invalid_snapshot() {
    const CHILD_ROOT: &str = "CASS_TEST_GH552_CONTINUE_ROOT";
    if let Ok(root) = dotenvy::var(CHILD_ROOT) {
        let root = PathBuf::from(root);
        for selected in [
            &root,
            &root.join("sessions"),
            &root.join("sessions/bad.json"),
        ] {
            let mut completed = 0;
            let mut delivered = 0;
            let error = ContinueConnector::new()
                .scan_allowed(
                    &context(selected),
                    &mut SourceScanHooks {
                        should_scan_source: Some(&mut |_| true),
                        on_source_complete: Some(&mut |_| {
                            completed += 1;
                            Ok(())
                        }),
                    },
                    &mut |_| {
                        delivered += 1;
                        Ok(())
                    },
                    &|_| false,
                )
                .unwrap_err();
            assert_eq!(completed, 0);
            assert_eq!(delivered, 0);
            assert!(
                error
                    .to_string()
                    .contains("1 admitted snapshot(s) did not complete")
            );
        }
        return;
    }
    // Isolate configuration in a subprocess instead of changing the shared
    // test process's environment while other connector tests are running.
    let temp = tempfile::Builder::new()
        .prefix("cass-gh552-configured-")
        .disable_cleanup(true)
        .tempdir()
        .unwrap();
    let root = temp.path().join("custom-store");
    fs::create_dir_all(root.join("sessions")).unwrap();
    fs::write(
        root.join("sessions/bad.json"),
        br#"{"title":"invalid Continue snapshot"}"#,
    )
    .unwrap();
    fs::create_dir(temp.path().join("unused")).unwrap();
    let configurations = vec![root.clone(), PathBuf::from("unused/../custom-store")];
    #[cfg(unix)]
    let configurations = {
        std::os::unix::fs::symlink(&root, temp.path().join("store-alias")).unwrap();
        let mut paths = configurations;
        paths.push(PathBuf::from("store-alias"));
        paths
    };
    for configured in configurations {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "connectors::continue_dev::tests::gh552_custom_continue_store_does_not_disclaim_invalid_snapshot",
                "--nocapture",
            ])
            .current_dir(temp.path())
            .env("CONTINUE_GLOBAL_DIR", &configured)
            .env(CHILD_ROOT, &root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{configured:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
    }
}

#[test]
fn excluded_sources_never_reach_discovery_admission_or_completion() {
    let temp = tempfile::tempdir().unwrap();
    let private = temp.path().join("private.json");
    seed(&private);
    seed(&temp.path().join("private-copy.json"));
    let excludes = |path: &Path| path.file_name().is_some_and(|name| name == "private.json");
    let connector = ContinueConnector::new();
    let ctx = context(temp.path());
    let sources = connector.discover_allowed(&ctx, &excludes).unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(
        sources[0].source_path.file_name().unwrap(),
        "private-copy.json"
    );
    let mut admitted = Vec::new();
    let mut completed = Vec::new();
    let mut conversations = Vec::new();
    connector
        .scan_allowed(
            &ctx,
            &mut SourceScanHooks {
                should_scan_source: Some(&mut |source| {
                    admitted.push(source.source_path.clone());
                    true
                }),
                on_source_complete: Some(&mut |completion| {
                    completed.push(completion.source.clone());
                    Ok(())
                }),
            },
            &mut |conversation| {
                conversations.push(conversation);
                Ok(())
            },
            &excludes,
        )
        .unwrap();
    assert_eq!(admitted, [sources[0].source_path.clone()]);
    assert_eq!(completed, sources);
    assert_eq!(conversations.len(), 1);
    assert_eq!(
        conversations[0].external_id.as_deref(),
        Some("private-copy")
    );
}

#[test]
fn all_selected_roots_excluded_does_not_scan_the_default_data_home() {
    let temp = tempfile::tempdir().unwrap();
    let implicit = temp.path().join(".continue");
    seed(&implicit.join("sessions/default.json"));
    let selected = temp.path().join("selected");
    seed(&selected.join("explicit.json"));
    let ctx = ScanContext::with_roots(implicit, vec![ScanRoot::local(selected.clone())], None);
    let excludes = |path: &Path| path.starts_with(&selected);
    let connector = ContinueConnector::new();
    assert!(selected_scope(&ctx, &excludes).is_none());
    assert!(
        connector
            .discover_allowed(&ctx, &excludes)
            .unwrap()
            .is_empty()
    );
    connector
        .scan_allowed(
            &ctx,
            &mut SourceScanHooks {
                should_scan_source: Some(&mut |_| panic!("excluded source was admitted")),
                on_source_complete: Some(&mut |_| panic!("excluded source completed")),
            },
            &mut |_| panic!("explicit empty selection fell through to default history"),
            &excludes,
        )
        .unwrap();
}

#[test]
fn durable_ledger_can_retry_old_sources_but_plain_scans_keep_their_cutoff() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("old.json");
    seed(&file);
    fs::File::open(&file)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(100))
        .unwrap();
    let mut ctx = context(temp.path());
    ctx.since_ts = Some(200_000);
    let connector = ContinueConnector::new();
    let delivered = Cell::new(0);
    let mut emit = |_| {
        delivered.set(delivered.get() + 1);
        Ok(())
    };
    connector
        .scan_allowed(&ctx, &mut SourceScanHooks::default(), &mut emit, &|_| false)
        .unwrap();
    assert_eq!(delivered.get(), 0);
    let mut completions = 0;
    connector
        .scan_allowed(
            &ctx,
            &mut SourceScanHooks {
                should_scan_source: Some(&mut |_| true),
                on_source_complete: Some(&mut |_| {
                    completions += 1;
                    Ok(())
                }),
            },
            &mut emit,
            &|_| false,
        )
        .unwrap();
    assert_eq!(delivered.get(), 1);
    assert_eq!(completions, 1);
    assert_eq!(
        ctx.since_ts,
        Some(200_000),
        "never mutate the caller's context"
    );
}

#[test]
fn rejected_ledger_source_is_not_parsed_or_certified() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("invalid.json"), b"{invalid").unwrap();
    ContinueConnector::new()
        .scan_allowed(
            &context(temp.path()),
            &mut SourceScanHooks {
                should_scan_source: Some(&mut |_| false),
                on_source_complete: Some(&mut |_| panic!("skipped source completed")),
            },
            &mut |_| panic!("skipped source parsed"),
            &|_| false,
        )
        .unwrap();
}

#[test]
fn policy_is_rechecked_after_the_host_admission_hook() {
    let temp = tempfile::tempdir().unwrap();
    seed(&temp.path().join("session.json"));
    let blocked = Cell::new(false);
    ContinueConnector::new()
        .scan_allowed(
            &context(temp.path()),
            &mut SourceScanHooks {
                should_scan_source: Some(&mut |_| {
                    blocked.set(true);
                    true
                }),
                on_source_complete: Some(&mut |_| panic!("newly excluded source completed")),
            },
            &mut |_| panic!("policy change before parsing was ignored"),
            &|_| blocked.get(),
        )
        .unwrap();
    assert!(blocked.get());
}

#[test]
fn malformed_sources_do_not_hide_successful_sources_or_certify_the_scan() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a-invalid.json"), b"{invalid").unwrap();
    seed(&temp.path().join("b-valid.json"));
    fs::write(temp.path().join("c-invalid.json"), b"{invalid").unwrap();
    let mut delivered = 0;
    let mut completed = 0;
    let error = ContinueConnector::new()
        .scan_allowed(
            &context(temp.path()),
            &mut SourceScanHooks {
                should_scan_source: Some(&mut |_| true),
                on_source_complete: Some(&mut |completion| {
                    assert_eq!(completion.conversations_emitted, 1);
                    assert_eq!(
                        completion.source.source_path.file_name().unwrap(),
                        "b-valid.json"
                    );
                    completed += 1;
                    Ok(())
                }),
            },
            &mut |_| {
                delivered += 1;
                Ok(())
            },
            &|_| false,
        )
        .unwrap_err();
    assert_eq!((delivered, completed), (1, 1));
    assert!(
        error
            .to_string()
            .contains("2 admitted snapshot(s) did not complete")
    );
    assert!(error.to_string().contains("a-invalid.json"));
}

#[test]
fn delivery_and_checkpoint_errors_preserve_the_original_error() {
    let temp = tempfile::tempdir().unwrap();
    seed(&temp.path().join("session.json"));
    let connector = ContinueConnector::new();
    for fail_delivery in [true, false] {
        let error = connector
            .scan_allowed(
                &context(temp.path()),
                &mut SourceScanHooks {
                    should_scan_source: None,
                    on_source_complete: Some(&mut |_| {
                        assert!(!fail_delivery, "failed delivery must not complete");
                        Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "checkpoint failure",
                        )
                        .into())
                    }),
                },
                &mut |_| {
                    if fail_delivery {
                        Err(
                            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "delivery failure")
                                .into(),
                        )
                    } else {
                        Ok(())
                    }
                },
                &|_| false,
            )
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }
}

#[test]
fn a_source_changed_during_delivery_is_not_a_successful_scan() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("session.json");
    seed(&source);
    let error = ContinueConnector::new()
        .scan_allowed(
            &context(temp.path()),
            &mut SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut |_| panic!("changed source completed")),
            },
            &mut |_| {
                fs::write(&source, b"{changed during delivery").unwrap();
                Ok(())
            },
            &|_| false,
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("1 admitted snapshot(s) did not complete")
    );
}

#[test]
fn actual_registry_factory_uses_ledger_first_admission() {
    let temp = tempfile::tempdir().unwrap();
    seed(&temp.path().join("session.json"));
    let mut ctx = context(temp.path());
    ctx.since_ts = Some(i64::MAX);
    let factories = crate::connectors::get_connector_factories();
    assert_eq!(
        factories
            .iter()
            .filter(|(name, _)| *name == "continue")
            .count(),
        1
    );
    let factory = factories
        .iter()
        .find(|(name, _)| *name == "continue")
        .unwrap()
        .1;
    let connector = factory();
    assert!(connector.supports_streaming_scan());
    assert!(connector.supports_source_boundaries());
    let mut delivered = 0;
    connector
        .scan_with_source_boundaries(
            &ctx,
            &mut SourceScanHooks {
                should_scan_source: Some(&mut |_| true),
                on_source_complete: None,
            },
            &mut |_| {
                delivered += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(
        delivered, 1,
        "the raw FAD factory loses this old source before its hook"
    );
}

#[test]
fn discovery_completion_and_conversations_keep_remote_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("session.json");
    seed(&file);
    let before = fs::read(&file).unwrap();
    let origin = Origin::remote("workstation");
    let root = ScanRoot::remote(temp.path().to_path_buf(), origin.clone(), None)
        .with_rewrite("/remote", "/local");
    let ctx = ScanContext::with_roots(temp.path().join("cass"), vec![root], None);
    let connector = ContinueConnector::new();
    let discovered = connector.discover_allowed(&ctx, &|_| false).unwrap();
    assert_eq!(discovered.len(), 1);
    let mut completed = Vec::new();
    connector
        .scan_allowed(
            &ctx,
            &mut SourceScanHooks {
                should_scan_source: Some(&mut |_| true),
                on_source_complete: Some(&mut |completion| {
                    completed.push(completion.source.clone());
                    Ok(())
                }),
            },
            &mut |conversation| {
                assert_eq!(conversation.agent_slug, "continue");
                assert_eq!(conversation.external_id.as_deref(), Some("session"));
                assert_eq!(
                    conversation.workspace,
                    Some(PathBuf::from("/local/project"))
                );
                assert_eq!(
                    conversation.metadata["origin"],
                    serde_json::to_value(&origin).unwrap()
                );
                Ok(())
            },
            &|_| false,
        )
        .unwrap();
    assert_eq!(completed, discovered);
    assert_eq!(completed[0].origin, origin);
    assert_eq!(fs::read(&file).unwrap(), before);
}
