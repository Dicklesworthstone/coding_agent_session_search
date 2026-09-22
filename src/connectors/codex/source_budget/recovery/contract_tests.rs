use super::*;
use crate::connectors::codex::CodexConnector;
use franken_agent_detection::{Origin, Platform};
use std::fs;
use std::path::PathBuf;

use super::tests::{corpus, message};

fn identities(conversations: Vec<NormalizedConversation>) -> Vec<(PathBuf, Option<String>)> {
    let mut identities: Vec<_> = conversations
        .into_iter()
        .map(|conversation| (conversation.source_path, conversation.external_id))
        .collect();
    identities.sort();
    identities
}

#[test]
fn scoped_scans_preserve_ids_for_every_original_root_shape() -> Result<()> {
    let (root, paths, ctx) = corpus()?;
    let codex = root.path().join(".codex");
    let sessions = codex.join("sessions");
    let day = paths[0].parent().unwrap().to_path_buf();
    let shapes = vec![
        vec![codex.clone()],
        vec![sessions.clone()],
        vec![day],
        vec![paths[0].clone()],
        vec![codex, sessions, paths[0].clone()],
    ];
    for shape in shapes {
        let scoped = ScanContext::with_roots(
            ctx.data_dir.clone(),
            shape.into_iter().map(ScanRoot::local).collect(),
            None,
        );
        let expected = identities(franken_agent_detection::CodexConnector::new().scan(&scoped)?);
        let actual = identities(CodexConnector::new().scan(&scoped)?);
        assert!(!expected.is_empty());
        assert_eq!(actual, expected, "root scope changed external IDs");
    }
    Ok(())
}

#[test]
fn custom_remote_roots_keep_distinct_ids_and_original_provenance() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mirror = root.path().join("custom-mirror");
    for name in ["alpha", "beta"] {
        let directory = mirror.join(name);
        fs::create_dir_all(&directory)?;
        fs::write(directory.join("rollout-same.jsonl"), message(name))?;
    }
    let ctx = ScanContext::with_roots(
        root.path().join("cass"),
        vec![
            ScanRoot::remote(mirror, Origin::remote("workstation"), Some(Platform::Linux))
                .with_rewrite("/remote/project", "/local/project"),
        ],
        None,
    );
    let inner = franken_agent_detection::CodexConnector::new();
    let inventory = inner.discover_source_files(&ctx)?;
    let expected = identities(inner.scan(&ctx)?);
    let mut complete = |done: &SourceCompletion| {
        assert!(inventory.contains(&done.source));
        assert_eq!(done.source.origin, Origin::remote("workstation"));
        assert_eq!(done.source.platform, Some(Platform::Linux));
        Ok(())
    };
    let mut hooks = SourceScanHooks {
        should_scan_source: None,
        on_source_complete: Some(&mut complete),
    };
    let mut delivered = Vec::new();
    CodexConnector::new().scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
        delivered.push(conversation);
        Ok(())
    })?;
    let actual = identities(delivered);
    assert_eq!(actual.len(), 2);
    assert_ne!(
        actual[0].1, actual[1].1,
        "distinct nested sessions collided"
    );
    assert_eq!(actual, expected);
    let scoped = scoped_context(&ctx, &inventory[0]);
    assert_eq!(
        scoped.scan_roots[0].rewrite_workspace("/remote/project/file", Some("codex")),
        "/local/project/file"
    );
    Ok(())
}

#[test]
fn replacing_an_inventory_file_with_a_directory_cannot_expand_scan_scope() -> Result<()> {
    let (_root, paths, ctx) = corpus()?;
    let hidden = paths[1].join("rollout-hidden.jsonl");
    let mut complete = |done: &SourceCompletion| -> Result<()> {
        if done.source.source_path == paths[0] {
            fs::rename(&paths[1], paths[1].with_extension("retained"))?;
            fs::create_dir(&paths[1])?;
            fs::write(&hidden, message("undiscovered-private-body"))?;
        }
        Ok(())
    };
    let mut predicate = |source: &DiscoveredSourceFile| {
        assert!(
            paths.contains(&source.source_path),
            "scope expanded to a child"
        );
        true
    };
    let mut hooks = SourceScanHooks {
        should_scan_source: Some(&mut predicate),
        on_source_complete: Some(&mut complete),
    };
    let mut delivered = Vec::new();
    let error = CodexConnector::new()
        .scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
            delivered.push(conversation.source_path);
            Ok(())
        })
        .unwrap_err();
    assert_eq!(delivered, [paths[0].clone(), paths[2].clone()]);
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
    assert_eq!(
        fs::read_to_string(hidden)?,
        message("undiscovered-private-body")
    );
    Ok(())
}

#[test]
fn reports_bound_samples_and_preserve_budget_only_schema() -> Result<()> {
    let path = PathBuf::from("private\n\"rollout.jsonl");
    let source = DiscoveredSourceFile::new(
        "codex",
        &ScanRoot::local(path.clone()),
        path.clone(),
        DiscoveredSourceRole::PrimarySessionLog,
        true,
    );
    for mixed in [false, true] {
        let mut failures = Failures::default();
        let count = MAX_REJECTION_SAMPLES + 8;
        for index in 0..count {
            let error = if mixed && index % 2 == 0 {
                io::Error::new(io::ErrorKind::UnexpectedEof, "raw-private-body").into()
            } else {
                IncompleteScan {
                    rejected_source_count: 1,
                    rejected_sources: vec![RejectedSource {
                        source_path: path.to_string_lossy().into_owned(),
                        observed_bytes: super::super::MAX_AUGMENT_ROLLOUT_BYTES + 1,
                        limit_bytes: None,
                    }],
                    ..IncompleteScan::default()
                }
                .into()
            };
            failures.record(&source, error);
        }
        let error = failures.finish().unwrap_err();
        let summary = error.to_string();
        assert!(!summary.contains('\n'));
        assert!(!summary.contains("raw-private-body"));
        let json: serde_json::Value =
            serde_json::from_str(summary.strip_prefix("Codex scan incomplete: ").unwrap())?;
        if mixed {
            assert_eq!(json["failed_source_count"], count);
            assert_eq!(
                json["failed_sources"].as_array().unwrap().len(),
                MAX_REJECTION_SAMPLES
            );
            assert!(error.downcast_ref::<io::Error>().is_some());
        } else {
            assert_eq!(json["reason"], "enrichment_read_budget_exceeded");
            assert_eq!(json["rejected_source_count"], count);
            assert_eq!(
                json["rejected_sources"].as_array().unwrap().len(),
                MAX_REJECTION_SAMPLES
            );
            assert!(error.downcast_ref::<IncompleteScan>().is_some());
        }
        assert_eq!(json["omitted_source_count"], 8);
    }
    Ok(())
}

#[test]
fn aggregation_keeps_each_formats_effective_limit_and_default_schema() -> Result<()> {
    let limits = ScanLimits {
        jsonl_bytes: 512 * 1024 * 1024,
    };
    let mut failures = Failures {
        budgets: limits.incomplete(),
        ..Failures::default()
    };
    for (name, cap) in [
        ("rollout-a.jsonl", limits.jsonl_bytes),
        ("rollout-b.json", super::super::MAX_AUGMENT_ROLLOUT_BYTES),
    ] {
        let path = PathBuf::from(name);
        let source = DiscoveredSourceFile::new(
            "codex",
            &ScanRoot::local(path.clone()),
            path,
            DiscoveredSourceRole::PrimarySessionLog,
            true,
        );
        let error = IncompleteScan {
            limit_bytes: cap,
            rejected_source_count: 1,
            rejected_sources: vec![RejectedSource {
                source_path: name.into(),
                observed_bytes: cap + 1,
                limit_bytes: None,
            }],
            ..IncompleteScan::default()
        };
        failures.record(&source, error.into());
    }
    let error = failures.finish().unwrap_err();
    let report = serde_json::to_value(error.downcast_ref::<IncompleteScan>().unwrap())?;
    assert_eq!(report["limit_bytes"], limits.jsonl_bytes);
    assert!(report["rejected_sources"][0].get("limit_bytes").is_none());
    assert_eq!(
        report["rejected_sources"][1]["limit_bytes"],
        super::super::MAX_AUGMENT_ROLLOUT_BYTES
    );
    Ok(())
}
