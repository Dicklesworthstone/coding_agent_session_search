//! Run the production canary's real-index regressions without compiling the
//! unrelated CLI unit-test monolith. No alternate query or fake engine is used.
//! Also exercise the real CLI's durable failure, append-only retry and audit.
pub use coding_agent_search::search;

// Preserve both the production module ancestry used by pub(in ...) and the
// ordinary file-module lookup that finds canary/exact.rs. Loading canary.rs
// directly with #[path] made its child resolve beside, not inside, canary/.
#[path = "../src/indexer"]
mod indexer {
    mod lexical_reconcile {
        mod canary;
    }
}

#[cfg(unix)]
mod cli_growth_tests {
    use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use coding_agent_search::search::tantivy::{TantivyIndex, expected_index_dir};
    use coding_agent_search::storage::sqlite::FrankenStorage;
    use serde_json::Value;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    fn command(root: &Path, id: i64) -> assert_cmd::Command {
        let mut command = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command
            .timeout(Duration::from_secs(60))
            .env_clear()
            .env("HOME", root)
            .env("USERPROFILE", root)
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_CONFIG_HOME", root.join(".config"))
            .env("XDG_DATA_HOME", root.join(".local/share"))
            .env("CASS_DATA_DIR", root.join("data"))
            .env("CASS_AUTO_REFRESH", "0")
            .env("RUST_MIN_STACK", "134217728")
            .current_dir(root)
            .arg("--db")
            .arg(root.join("archive.db"))
            .args([
                "index",
                "--reconcile-conversation",
                &id.to_string(),
                "--json",
            ])
            .arg("--data-dir")
            .arg(root.join("data"));
        command
    }

    #[test]
    fn cli_repair_rebinds_appended_rows_after_a_durable_prepublication_failure() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let data = root.join("data");
        let db = root.join("archive.db");
        let storage = FrankenStorage::open(&db).unwrap();
        let agent = storage
            .ensure_agent(&Agent {
                id: None,
                slug: "codex".into(),
                name: "Codex".into(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();
        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some("repair-growth".into()),
            title: Some("growth repair".into()),
            source_path: root.join("source.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: None,
            approx_tokens: None,
            metadata_json: Value::Null,
            messages: (0..6)
                .map(|idx| Message {
                    id: None,
                    idx,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(1_700_000_000_000 + idx),
                    content: format!("meridian durable prefix {idx}"),
                    extra_json: Value::Null,
                    snippets: Vec::new(),
                })
                .collect(),
            source_id: "local".into(),
            origin_host: None,
        };
        let id = storage
            .insert_conversation_tree(agent, None, &conversation)
            .unwrap()
            .conversation_id;
        drop(storage);
        let index_path = expected_index_dir(&data);
        let mut index = TantivyIndex::open_or_create(&index_path).unwrap();
        index.commit().unwrap();
        drop(index);
        let checkpoint = index_path.join(format!(".lexical-reconcile-{id}.json"));
        let progress = checkpoint.with_extension("progress.json");
        // An invalid progress entry causes a real CLI failure AFTER the source
        // checkpoint is durable, before any source documents are published.
        fs::create_dir(&progress).unwrap();
        command(root, id).assert().code(5);
        let previous: Value = serde_json::from_slice(&fs::read(&checkpoint).unwrap()).unwrap();
        assert_eq!(previous["expected_docs"], 6);
        assert_eq!(previous["attempt"], 1);
        fs::rename(&progress, index_path.join("retained-invalid-progress")).unwrap();
        let storage = FrankenStorage::open(&db).unwrap();
        storage.raw().execute_with_params(
            "INSERT INTO messages(conversation_id, idx, role, content, created_at) VALUES (?1, 9, 'user', 'meridian appended tail', 1700000000009)",
            &[id.into()],
        ).unwrap();
        drop(storage);
        let before = fs::read(&db).unwrap();
        let output = command(root, id).assert().success().get_output().clone();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["expected_docs"], 7, "{report}");
        assert_eq!(report["verified_docs"], 7);
        assert_eq!(report["attempt"], 2);
        assert_eq!(report["doc_count_after"], 7);
        assert_eq!(report["checkpoint_cleared"], true);
        assert!(!checkpoint.exists() && !progress.exists());
        assert_eq!(fs::read(&db).unwrap(), before);
        let reopened = TantivyIndex::open_or_create(&index_path).unwrap();
        assert_eq!(reopened.doc_count().unwrap(), 7);
        drop(reopened);
        let output = command(root, id).assert().success().get_output().clone();
        let again: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            again["doc_count_after"], 7,
            "repetition cannot duplicate messages"
        );
        assert_eq!(again["verified_docs"], 7);
    }
}
