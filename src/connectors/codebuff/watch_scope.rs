//! Translate a targeted metadata event to its one transcript, not a store scan.

use std::path::{Path, PathBuf};

/// FAD recognizes exact transcript roots but not exact run-state or chat roots.
/// Only translate the native chat slot. FAD still validates the transcript path
/// and owns parsing, identity and timestamps; unrelated JSON/directories retain
/// their existing interpretation. Do not follow a run-state symlink to choose
/// the chat: reconstruction reads the transcript beside the selected sidecar.
pub(super) fn transcript_selector(path: &Path) -> PathBuf {
    let chat = if path
        .file_name()
        .is_some_and(|name| name == "run-state.json")
    {
        path.parent()
    } else if path.is_dir() {
        Some(path)
    } else {
        None
    };
    let Some(chat) = chat else {
        return path.to_path_buf();
    };
    let native_slot = chat
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "chats"))
        .and_then(Path::parent)
        .and_then(Path::parent)
        .is_some_and(|parent| parent.file_name().is_some_and(|name| name == "projects"));
    let transcript = chat.join("chat-messages.json");
    if native_slot && transcript.is_file() {
        transcript
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::codebuff::CodebuffConnector;
    use crate::connectors::{Connector, ScanContext, ScanRoot};
    use franken_agent_detection::connectors::SourceScanHooks;
    use serde_json::json;
    use std::fs;

    fn transcript(projects: &Path, project: &str) -> PathBuf {
        let chat = projects
            .join(project)
            .join("chats/2026-03-21T17-14-03.768Z");
        fs::create_dir_all(&chat).unwrap();
        let transcript = chat.join("chat-messages.json");
        fs::write(
            &transcript,
            serde_json::to_vec(&json!([
                {"id":"user-1774113351457", "variant":"user",
                 "content":"metadata scope", "timestamp":"01:15 PM"}
            ]))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            chat.join("run-state.json"),
            br#"{"sessionState":{"fileContext":{"projectRoot":"/changed/workspace"}}}"#,
        )
        .unwrap();
        transcript
    }

    #[test]
    fn metadata_and_chat_selectors_read_only_their_native_chat() {
        for directory in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let projects = temp.path().join("projects");
            let primary = transcript(&projects, "selected");
            let other = transcript(&projects, "unrequested");
            fs::write(&other, b"[").unwrap();
            let selected = if directory {
                primary.parent().unwrap().to_path_buf()
            } else {
                primary.with_file_name("run-state.json")
            };
            let root = ScanRoot::local(selected.clone());
            let ctx = ScanContext::with_roots(temp.path().join("data"), vec![root], None);
            // Actual pre-fix behavior: FAD sees no transcript for either of
            // these selectors. A broad store scan fails on the other chat.
            let upstream = franken_agent_detection::CodebuffConnector::new();
            assert!(upstream.scan(&ctx).unwrap().is_empty());
            let broad = ScanContext::with_roots(
                temp.path().join("data"),
                vec![ScanRoot::local(projects)],
                None,
            );
            assert!(upstream.scan(&broad).is_err());

            let connector = CodebuffConnector::new();
            let discovered = connector.discover_source_files(&ctx).unwrap();
            assert_eq!(discovered.len(), 2);
            assert!(discovered.iter().all(|source| source.scan_root == selected));
            let mut delivered = Vec::new();
            let mut completed = Vec::new();
            connector
                .scan_with_source_boundaries(
                    &ctx,
                    &mut SourceScanHooks {
                        should_scan_source: None,
                        on_source_complete: Some(&mut |done| {
                            completed.push(done.clone());
                            Ok(())
                        }),
                    },
                    &mut |conversation| {
                        delivered.push(conversation);
                        Ok(())
                    },
                )
                .unwrap();
            assert_eq!(delivered.len(), 1);
            assert_eq!(delivered[0].source_path, primary);
            assert_eq!(
                delivered[0].workspace.as_deref(),
                Some(Path::new("/changed/workspace"))
            );
            assert_eq!(delivered[0].messages[0].created_at, Some(1_774_113_351_457));
            assert_eq!(completed.len(), 1);
            assert_eq!(completed[0].source.scan_root, selected);
            assert_eq!(completed[0].source, discovered[0]);
            assert_eq!(completed[0].required_sidecars, vec![discovered[1].clone()]);
        }
    }

    #[test]
    fn overlapping_selectors_preserve_the_first_roots_provenance() {
        for sidecar_first in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let primary = transcript(&temp.path().join("projects"), "selected");
            let sidecar = primary.with_file_name("run-state.json");
            let paths = if sidecar_first {
                [sidecar, primary.clone()]
            } else {
                [primary.clone(), sidecar]
            };
            let ctx = ScanContext::with_roots(
                temp.path().join("data"),
                paths.iter().cloned().map(ScanRoot::local).collect(),
                None,
            );
            let connector = CodebuffConnector::new();
            let discovered = connector.discover_source_files(&ctx).unwrap();
            assert_eq!(
                discovered.len(),
                2,
                "overlapping selectors must deduplicate"
            );
            assert!(discovered.iter().all(|source| source.scan_root == paths[0]));
            assert_eq!(connector.scan(&ctx).unwrap().len(), 1);
        }
    }

    #[test]
    fn missing_primary_and_unrelated_metadata_do_not_widen_scope() {
        let temp = tempfile::tempdir().unwrap();
        let primary = transcript(&temp.path().join("projects"), "selected");
        let chat = primary.parent().unwrap();
        let unrelated = chat.join("other.json");
        fs::write(&unrelated, b"{}").unwrap();
        assert_eq!(transcript_selector(&unrelated), unrelated);
        assert_eq!(transcript_selector(&primary), primary);
        assert_eq!(transcript_selector(temp.path()), temp.path());
        let missing = temp.path().join("projects/empty/chats/chat/run-state.json");
        fs::create_dir_all(missing.parent().unwrap()).unwrap();
        fs::write(&missing, b"{}").unwrap();
        assert_eq!(transcript_selector(&missing), missing);
        let foreign = temp.path().join("not-projects/p/chats/c");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("chat-messages.json"), b"[]").unwrap();
        let state = foreign.join("run-state.json");
        fs::write(&state, b"{}").unwrap();
        assert_eq!(transcript_selector(&state), state);
    }
}
