//! Root narrowing must preserve the real Codex connector's source coverage.

use super::*;
use std::time::{Duration, UNIX_EPOCH};

const ROLLOUT: &str = concat!(
    "{\"type\":\"event_msg\",\"timestamp\":1792108800000,\"payload\":{\"type\":\"user_message\",\"message\":\"compressed history sentinel\"}}\n",
    "{\"type\":\"response_item\",\"timestamp\":1792108801000,\"payload\":{\"role\":\"assistant\",\"content\":\"retained reply\"}}\n",
);

fn seed(path: &Path, compressed: bool) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = if compressed {
        // zstd-compressed ROLLOUT; the real pinned FAD decoder reads this in
        // the scan comparisons below, not a fake decompressor or file probe.
        hex::decode(concat!(
            "28b52ffd20e6650400d2891d1b90a5cd019821fa62639304b3ad6623c85d07f43bf80",
            "cf2d4f55a2ec37b7689bd11ca13e1bef978f39b2bb1f126ae22554cab19ea8b906b2",
            "ea66fc37b4e4fb479255c5bab884028577d636a253e36fe82aa5748bb168e155cc921",
            "b3298ea33002038171cc14ae5ea1b5f1a20ad93cc07d7ce6d6010107002018402e80",
            "d51f22e8a785d4ad6a26007a",
        ))
        .unwrap()
    } else {
        ROLLOUT.as_bytes().to_vec()
    };
    fs::write(path, bytes).unwrap();
}

fn scanned(roots: Vec<ScanRoot>, data: &Path) -> Vec<(PathBuf, Option<String>, Vec<String>)> {
    let context = ScanContext::with_roots(data.to_path_buf(), roots, None);
    let connector = codex::CodexConnector::new();
    let mut rows: Vec<_> = connector
        .scan(&context)
        .unwrap()
        .into_iter()
        .map(|conversation| {
            (
                conversation.source_path,
                conversation.external_id,
                conversation
                    .messages
                    .into_iter()
                    .map(|message| message.content)
                    .collect(),
            )
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn compressed_only_preflight_matches_actual_directory_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".codex");
    for name in ["rollout-lower.jsonl.zst", "rollout-upper.JSONL.ZST"] {
        seed(&home.join("sessions/2026/10/16").join(name), true);
    }
    fs::write(home.join("sessions/notes.jsonl.zst"), b"not a rollout").unwrap();
    let root = ScanRoot::local(home.clone());
    let preflight = preflight_codex_explicit_file_roots(std::slice::from_ref(&root), None);
    assert_eq!(preflight.explicit_file_roots, 2);
    assert_eq!(preflight.fallback_roots, 0);
    let expected = scanned(vec![root], &tmp.path().join("cass"));
    assert_eq!(expected.len(), 2, "the fixture must exercise real decoding");
    assert!(expected.iter().all(|row| {
        row.2.iter().any(|text| text.contains("compressed history sentinel"))
    }));
    assert_eq!(
        scanned(preflight.scan_roots, &tmp.path().join("cass")),
        expected
    );
}

#[test]
fn plain_and_compressed_siblings_remain_one_session() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".codex");
    let plain = home.join("sessions/rollout-pair.jsonl");
    seed(&plain, false);
    seed(&home.join("sessions/rollout-pair.jsonl.zst"), true);
    let roots = vec![ScanRoot::local(home)];
    let preflight = preflight_codex_explicit_file_roots(&roots, None);
    assert_eq!(preflight.explicit_file_roots, 1);
    assert_eq!(preflight.scan_roots[0].path, plain);
    let expected = scanned(roots, &tmp.path().join("cass"));
    assert_eq!(expected.len(), 1);
    assert_eq!(
        scanned(preflight.scan_roots, &tmp.path().join("cass")),
        expected
    );
}

#[test]
fn representation_preference_precedes_the_incremental_cutoff() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let plain = sessions.join("rollout-pair.jsonl");
    let compressed = sessions.join("rollout-pair.jsonl.zst");
    seed(&plain, false);
    seed(&compressed, true);
    fs::File::open(&plain)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(100))
        .unwrap();
    fs::File::open(&compressed)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(300))
        .unwrap();
    assert!(
        collect_codex_rollout_files(&sessions, Some(200_000))
            .unwrap()
            .is_empty()
    );
    let preflight = preflight_codex_explicit_file_roots(
        &[ScanRoot::local(sessions.clone())],
        Some(200_000),
    );
    assert_eq!(preflight.fallback_roots, 1);
    assert_eq!(preflight.scan_roots[0].path, sessions);
}

#[test]
fn full_home_retains_archived_rollouts_and_their_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".codex");
    seed(&home.join("sessions/rollout-active.jsonl"), false);
    seed(
        &home.join("archived_sessions").join(concat!(
            "rollout-2026-10-15T10-20-30-",
            "12345678-1234-1234-1234-123456789abc.jsonl.zst",
        )),
        true,
    );
    let roots = vec![ScanRoot::local(home.clone())];
    let preflight = preflight_codex_explicit_file_roots(&roots, None);
    assert_eq!(preflight.fallback_roots, 1);
    assert_eq!(preflight.scan_roots[0].path, home);
    let expected = scanned(roots, &tmp.path().join("cass"));
    assert_eq!(expected.len(), 2);
    assert!(expected.iter().any(|row| row.1.as_deref().is_some_and(|id| {
        id.starts_with("2026/10/15/") || id.starts_with("2026\\10\\15\\")
    })));
    assert_eq!(
        scanned(preflight.scan_roots, &tmp.path().join("cass")),
        expected
    );
}

#[test]
fn explicitly_selected_sessions_do_not_expand_to_the_sibling_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".codex");
    let active = home.join("sessions/rollout-active.jsonl.zst");
    seed(&active, true);
    seed(&home.join("archived_sessions/rollout-archive.jsonl"), false);
    let preflight =
        preflight_codex_explicit_file_roots(&[ScanRoot::local(home.join("sessions"))], None);
    assert_eq!(preflight.fallback_roots, 0);
    assert_eq!(preflight.scan_roots.len(), 1);
    assert_eq!(preflight.scan_roots[0].path, active);
}

#[test]
fn empty_selected_roots_never_turn_into_implicit_default_discovery() {
    let tmp = tempfile::tempdir().unwrap();
    let empty = tmp.path().join("empty/sessions");
    let missing = tmp.path().join("missing/sessions");
    fs::create_dir_all(&empty).unwrap();
    for selected in [empty, missing] {
        let preflight =
            preflight_codex_explicit_file_roots(&[ScanRoot::local(selected.clone())], None);
        assert_eq!(preflight.fallback_roots, 1);
        assert_eq!(preflight.explicit_file_roots, 0);
        assert_eq!(preflight.scan_roots.len(), 1);
        assert_eq!(preflight.scan_roots[0].path, selected);
        let context =
            ScanContext::with_roots(tmp.path().join("cass"), preflight.scan_roots, None);
        assert!(!context.use_default_detection());
    }
}

#[test]
fn compressed_file_roots_and_expanded_roots_preserve_provenance() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let compressed = sessions.join("rollout-remote.jsonl.zst");
    seed(&compressed, true);
    for path in [sessions, compressed.clone()] {
        let root = ScanRoot::remote(path, Origin::remote("source-a"), None)
            .with_rewrite("/remote/work", "/local/work");
        let preflight = preflight_codex_explicit_file_roots(&[root], None);
        assert_eq!(preflight.explicit_file_roots, 1);
        let expanded = &preflight.scan_roots[0];
        assert_eq!(expanded.path, compressed);
        assert_eq!(expanded.origin, Origin::remote("source-a"));
        assert_eq!(
            expanded.rewrite_workspace("/remote/work/project", Some("codex")),
            "/local/work/project"
        );
    }
}

#[cfg(unix)]
#[test]
fn uncertain_archive_entries_keep_the_authoritative_home_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("custom-profile");
    seed(&home.join("sessions/rollout-active.jsonl"), false);
    std::os::unix::fs::symlink(home.join("missing"), home.join("archived_sessions")).unwrap();
    let preflight = preflight_codex_explicit_file_roots(&[ScanRoot::local(home.clone())], None);
    assert_eq!(preflight.fallback_roots, 1);
    assert_eq!(preflight.scan_roots[0].path, home);
}
