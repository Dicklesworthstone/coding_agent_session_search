use super::*;
use crate::connectors::codebuff::CodebuffConnector;
use crate::connectors::{Connector, DiscoveredSourceRole, Platform};
use franken_agent_detection::connectors::SourceScanHooks;
use serde_json::json;
use std::collections::HashSet;
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

fn observed(root: &ScanRoot, path: PathBuf) -> DiscoveredSourceFile {
    DiscoveredSourceFile::new(
        "codebuff",
        root,
        path,
        DiscoveredSourceRole::PrimarySessionLog,
        true,
    )
}

fn assert_same(actual: &ScanContext, expected: &ScanContext) {
    assert_eq!(actual.data_dir, expected.data_dir);
    assert_eq!(actual.since_ts, expected.since_ts);
    assert_eq!(actual.scan_roots.len(), 1);
    assert_eq!(expected.scan_roots.len(), 1);
    let actual_root = &actual.scan_roots[0];
    let expected_root = &expected.scan_roots[0];
    assert_eq!(actual_root.path, expected_root.path);
    assert_eq!(actual_root.origin, expected_root.origin);
    assert_eq!(actual_root.platform, expected_root.platform);
    assert_eq!(actual_root.workspace_rewrites, expected_root.workspace_rewrites);
    assert_eq!(
        actual_root.rewrite_workspace("/remote/project/src/main.rs", Some("codebuff")),
        expected_root.rewrite_workspace("/remote/project/src/main.rs", Some("codebuff")),
    );
    match (&actual.progress_tick, &expected.progress_tick) {
        (None, None) => {}
        (Some(actual), Some(expected)) => assert!(Arc::ptr_eq(actual, expected)),
        _ => panic!("narrowing changed the progress hook"),
    }
}

#[test]
fn gh511_context_index_preserves_complete_origin_and_first_selected_mapping() {
    let path = PathBuf::from("selected-store");
    let remote = Origin::remote_with_host("same-id", "host-a");
    let first = ScanRoot::remote(path.clone(), remote.clone(), Some(Platform::Macos))
        .with_rewrite("/remote", "/mirror/first");
    let duplicate = first.clone().with_rewrite("/remote/project", "/must-not-win");
    let other_host = ScanRoot::remote(
        path.clone(),
        Origin::remote_with_host("same-id", "host-b"),
        Some(Platform::Linux),
    )
    .with_rewrite("/remote", "/mirror/host-b");
    let local = ScanRoot::remote(
        path.clone(),
        Origin { source_id: remote.source_id, kind: SourceKind::Local, host: remote.host },
        Some(Platform::Windows),
    )
    .with_rewrite("/remote", "/mirror/local");
    let other_path = ScanRoot::local(PathBuf::from("other-store"))
        .with_rewrite("/remote", "/mirror/other");
    let ctx = ScanContext::with_roots(
        PathBuf::from("unchanged-cass-data"),
        vec![first, duplicate, other_host, local, other_path],
        Some(4_102_444_800_000),
    );
    let mut indexed = SourceContexts::new(&ctx);
    assert_eq!(indexed.roots.len(), 4, "duplicate selections keep their first owner");
    for index in [3, 0, 2, 4, 1, 2, 0] {
        let root = &ctx.scan_roots[index];
        let source = observed(root, root.path.join("projects/p/chats/c/chat-messages.json"));
        let legacy = CodebuffConnector::source_context(&ctx, &source);
        assert_same(indexed.for_source(&source), &legacy);
    }
}

#[test]
fn gh511_context_reuse_keeps_one_slot_and_never_leaks_previous_provenance() {
    let ticks = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&ticks);
    let ctx = ScanContext::with_roots(
        PathBuf::from("cass-data"),
        vec![ScanRoot::local(PathBuf::from("selected"))
            .with_rewrite("/remote", "/mapped")],
        Some(i64::MAX),
    )
    .with_progress_tick(Arc::new(move || { count.fetch_add(1, Ordering::SeqCst); }));
    let mut indexed = SourceContexts::new(&ctx);
    let first = observed(&ctx.scan_roots[0], PathBuf::from("selected/first/chat-messages.json"));
    let (slot, rewrites) = {
        let single = indexed.for_source(&first);
        assert_same(single, &CodebuffConnector::source_context(&ctx, &first));
        single.progress_tick.as_ref().unwrap()();
        (single.scan_roots.as_ptr(), single.scan_roots[0].workspace_rewrites.as_ptr())
    };
    let second = observed(&ctx.scan_roots[0], PathBuf::from("selected/second/chat-messages.json"));
    {
        let single = indexed.for_source(&second);
        assert_eq!(single.scan_roots.as_ptr(), slot);
        assert_eq!(single.scan_roots[0].workspace_rewrites.as_ptr(), rewrites);
        assert_same(single, &CodebuffConnector::source_context(&ctx, &second));
        single.progress_tick.as_ref().unwrap()();
    }
    // Default-detected/otherwise unregistered sources must not inherit the
    // preceding explicit root's workspace map, origin, platform or selector.
    for origin in [Origin::remote("unregistered"), Origin::local()] {
        let unknown = ScanRoot::remote(PathBuf::from("unregistered"), origin, None);
        let source = observed(&unknown, PathBuf::from("unregistered/chat-messages.json"));
        let single = indexed.for_source(&source);
        assert_same(single, &CodebuffConnector::source_context(&ctx, &source));
        assert!(single.scan_roots[0].workspace_rewrites.is_empty());
        assert_eq!(single.scan_roots.as_ptr(), slot);
    }
    assert_same(indexed.for_source(&first), &CodebuffConnector::source_context(&ctx, &first));
    assert_eq!(ticks.load(Ordering::SeqCst), 2);
    assert_eq!(ctx.since_ts, Some(i64::MAX));
    assert_eq!(ctx.scan_roots[0].path, Path::new("selected"));
    let default = ScanContext::local_default(PathBuf::from("default-data"), Some(9));
    let mut indexed_default = SourceContexts::new(&default);
    assert_same(
        indexed_default.for_source(&first),
        &CodebuffConnector::source_context(&default, &first),
    );
}

#[test]
fn gh511_many_root_preparation_has_a_live_legacy_comparator_and_one_owned_root() {
    let count = 256;
    let roots: Vec<_> = (0..count)
        .map(|index| {
            ScanRoot::remote(
                PathBuf::from(format!("selected-{index}")),
                Origin::remote_with_host(format!("source-{index}"), format!("host-{index}")),
                Some(Platform::Linux),
            )
            .with_rewrite("/remote", format!("/mirror/{index}/{}", "x".repeat(256)))
        })
        .collect();
    let sources: Vec<_> = roots.iter()
        .map(|root| observed(root, root.path.join("projects/p/chats/c/chat-messages.json")))
        .collect();
    let ctx = ScanContext::with_roots(PathBuf::from("data"), roots, Some(i64::MAX));
    let mut indexed = SourceContexts::new(&ctx);
    assert_eq!(indexed.roots.len(), count);
    // Every map entry borrows a caller-owned root rather than cloning its
    // paths, provenance and rewrite vectors into a second selection inventory.
    for root in &ctx.scan_roots {
        assert!(std::ptr::eq(
            *indexed.roots.get(&RootKey::new(&root.path, &root.origin)).unwrap(),
            root,
        ));
    }
    let slot_capacity = indexed.single.scan_roots.capacity();
    let mut old_time = Duration::ZERO;
    let mut new_time = Duration::ZERO;
    for round in 0..3 {
        // Alternate traversal order; this is preparation-only evidence, not
        // an end-to-end ingestion benchmark or an enforced timing threshold.
        for position in 0..count {
            let index = if round % 2 == 0 { position } else { count - position - 1 };
            let source = black_box(&sources[index]);
            let start = Instant::now();
            let old = black_box(CodebuffConnector::source_context(&ctx, source));
            old_time += start.elapsed();
            let start = Instant::now();
            let new = black_box(indexed.for_source(source));
            new_time += start.elapsed();
            assert_same(new, &old);
            assert_eq!(new.scan_roots.len(), 1);
            assert_eq!(new.scan_roots.capacity(), slot_capacity);
        }
    }
    eprintln!(
        "codebuff context preparation: roots={count} preparations={} legacy_ns={} indexed_ns={}; not whole-ingest timing",
        count * 3, old_time.as_nanos(), new_time.as_nanos(),
    );
}

fn write_old(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::File::options().write(true).open(path).unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_774_113_351)).unwrap();
}

#[test]
fn gh511_many_selected_transcripts_keep_fad_output_completions_and_old_failure_resume() {
    let temp = tempfile::tempdir().unwrap();
    let projects = temp.path().join("projects");
    let mut roots = Vec::new();
    let mut originals = Vec::new();
    for index in 0..48 {
        let chat = projects.join(format!("project-{index:03}"))
            .join("chats/2026-03-21T17-14-03.768Z");
        fs::create_dir_all(&chat).unwrap();
        let primary = chat.join("chat-messages.json");
        let bytes = serde_json::to_vec(&json!([{
            "id":"user-1774113351457", "variant":"user",
            "content":format!("contextrootproof{index:03}z"), "timestamp":"01:15 PM"
        }])).unwrap();
        write_old(&primary, &bytes);
        write_old(&chat.join("run-state.json"),
            br#"{"sessionState":{"fileContext":{"projectRoot":"/remote/project"}}}"#);
        roots.push(ScanRoot::remote(
            if index % 2 == 0 { primary.clone() } else { chat.join("run-state.json") },
            Origin::remote_with_host(format!("source-{index}"), format!("host-{index}")),
            Some(Platform::Linux),
        ).with_rewrite("/remote", format!("/mirror/{index}")));
        originals.push((primary, bytes));
    }
    roots.reverse();
    let ctx = ScanContext::with_roots(temp.path().join("data"), roots, None);
    let connector = CodebuffConnector::new();
    let sources = connector.discover_source_files(&ctx).unwrap();
    assert_eq!(sources.len(), originals.len() * 2);
    let bad = &originals[17].0;
    write_old(bad, b"[");
    // Keep the original constructor and actual upstream parser live in the
    // same invocation, rather than generating expected output from the index.
    let mut expected = Vec::new();
    for source in sources.iter().filter(|source| source.role == DiscoveredSourceRole::PrimarySessionLog) {
        let single = CodebuffConnector::source_context(&ctx, source);
        let result = franken_agent_detection::CodebuffConnector::new()
            .scan_with_callback(&single, &mut |conversation| { expected.push(conversation); Ok(()) });
        assert_eq!(result.is_err(), &source.source_path == bad);
    }
    let mut actual = Vec::new();
    let mut completions = Vec::new();
    let error = connector.scan_with_source_boundaries(
        &ctx,
        &mut SourceScanHooks {
            should_scan_source: None,
            on_source_complete: Some(&mut |done| { completions.push(done.clone()); Ok(()) }),
        },
        &mut |conversation| { actual.push(conversation); Ok(()) },
    ).unwrap_err();
    assert!(error.chain().any(|cause| cause.is::<serde_json::Error>()));
    assert!(error.to_string().contains(bad.to_string_lossy().as_ref()));
    assert_eq!(actual.len(), 47);
    assert_eq!(serde_json::to_value(&actual).unwrap(), serde_json::to_value(&expected).unwrap());
    assert_eq!(completions.len(), actual.len());
    for done in &completions {
        let source = sources.iter().find(|source| source.source_path == done.source.source_path).unwrap();
        assert_eq!(&done.source, source);
        assert_eq!(done.conversations_emitted, 1);
        assert_eq!(done.required_sidecars.len(), 1);
        assert_eq!(done.required_sidecars[0].source_path, done.source.source_path.with_file_name("run-state.json"));
        assert_eq!(done.required_sidecars[0].origin, done.source.origin);
    }
    let completed: HashSet<_> = completions.iter().map(|done| done.source.source_path.clone()).collect();
    write_old(bad, &originals[17].1);
    let mut retry = ctx.clone();
    retry.since_ts = Some(i64::MAX);
    let mut recovered = Vec::new();
    let mut recovered_completions = Vec::new();
    connector.scan_with_source_boundaries(
        &retry,
        &mut SourceScanHooks {
            should_scan_source: Some(&mut |source| !completed.contains(&source.source_path)),
            on_source_complete: Some(&mut |done| { recovered_completions.push(done.clone()); Ok(()) }),
        },
        &mut |conversation| { recovered.push(conversation); Ok(()) },
    ).unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(&recovered[0].source_path, bad);
    assert_eq!(recovered[0].messages[0].created_at, Some(1_774_113_351_457));
    assert_eq!(recovered_completions.len(), 1);
    assert_eq!(recovered_completions[0].source.origin, sources.iter().find(|source| &source.source_path == bad).unwrap().origin);
    for (path, bytes) in originals {
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), UNIX_EPOCH + Duration::from_secs(1_774_113_351));
    }
}
