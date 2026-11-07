//! GH #487: native OpenClaw transcripts through the application's connector
//! registry and the real `cass` binary.
//!
//! These tests need the full crate (`get_connector_factories`,
//! `DiscoveredSourceRole` via `coding_agent_search::connectors`, `assert_cmd`,
//! `cargo_bin!("cass")`), so they are their own target: the slim consumer
//! contract crate built by `scripts/test_openclaw_contract.py` compiles
//! `connector_openclaw_sqlite.rs` and cannot link them (bead
//! coding_agent_session_search-2l1b0.62).

mod registry_and_cli {
    //! Native OpenClaw consumer coverage, including the actual registry and CLI.
    //! Generated transcript_events fixtures use the published FAD 0.3.0 schema.
    //! These tests must not be feature-gated: disabling SQLite support in an ordinary
    //! CASS build must fail positive discovery/retrieval assertions, not skip them.

    use anyhow::{Context, Result, ensure};
    use coding_agent_search::connectors::{
        Connector, DiscoveredSourceRole, ScanContext, ScanRoot, get_connector_factories,
    };
    use coding_agent_search::franken_sync::Connection;
    use coding_agent_search::franken_sync::compat::{ConnectionExt, ParamValue, RowExt};
    use coding_agent_search::storage::sqlite::FrankenStorage;
    use serde::Deserialize;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const AT: i64 = 1_780_000_000_000;
    type SourceBundle = Vec<Option<(Vec<u8>, SystemTime)>>;

    // Exclude ranking scores and timing diagnostics: these can change when a
    // sibling is indexed, while canonical identity and historical content must not.
    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct SearchMessage {
        conversation_id: i64,
        line_number: usize,
        source_id: String,
        source_path: PathBuf,
        agent: String,
        created_at: Option<i64>,
        content: String,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct CanonicalMessage {
        conversation_id: i64,
        message_id: i64,
        idx: i64,
        created_at: Option<i64>,
        content: String,
    }

    fn connector() -> Box<dyn Connector + Send> {
        let (_, factory) = get_connector_factories()
            .into_iter()
            .find(|(name, _)| *name == "openclaw")
            .expect("OpenClaw must be registered in standard CASS builds");
        factory()
    }

    fn context(home: &Path, root: &Path) -> ScanContext {
        ScanContext::with_roots(
            home.join("cass-data"),
            vec![ScanRoot::local(root.to_path_buf())],
            None,
        )
    }

    fn append(conn: &Connection, session: &str, seq: i64, event: Value) -> Result<()> {
        let event = event.to_string();
        conn.execute_compat(
            "INSERT INTO transcript_events (session_id, seq, event_json, created_at)
         VALUES (?1, ?2, ?3, ?4)",
            &[
                ParamValue::from(session),
                ParamValue::from(seq),
                ParamValue::from(event.as_str()),
                ParamValue::from(AT + seq * 1_000),
            ],
        )?;
        Ok(())
    }

    fn fixture(home: &Path, agent: &str, session: &str, needle: &str) -> Result<PathBuf> {
        let directory = home.join(".openclaw/agents").join(agent).join("agent");
        fs::create_dir_all(&directory)?;
        let path = directory.join("openclaw-agent.sqlite");
        let conn = Connection::open(path.to_string_lossy().as_ref())?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
         PRAGMA wal_autocheckpoint = 0;
         CREATE TABLE transcript_events (
             session_id TEXT NOT NULL, seq INTEGER NOT NULL,
             event_json TEXT NOT NULL, created_at INTEGER NOT NULL,
             PRIMARY KEY (session_id, seq));",
        )?;
        // Deliberately insert out of sequence: event order comes from seq, not row
        // insertion order, and indices in normalized messages must be contiguous.
        append(
            &conn,
            session,
            5,
            json!({
                "type": "message", "message": {
                    "role": "assistant", "model": "fixture-model",
                    "content": [{"type": "text", "text": "Answer with Unicode: 東京 🚀"},
                                {"type": "toolCall", "name": "read_file", "id": "call-5",
                                 "arguments": {"path": "src/lib.rs"}}]
                }
            }),
        )?;
        append(
            &conn,
            session,
            0,
            json!({
                "type": "session", "cwd": "/work/native-history"
            }),
        )?;
        append(
            &conn,
            session,
            1,
            json!({
                // OpenClaw uses the first line as the conversation title, which
                // lexical search indexes on every message. Keep the needle out
                // of that title so exactly one content hit proves no duplication.
                "type": "message", "message": {
                    "role": "user", "content": format!("Historical OpenClaw transcript\n{needle}")
                }
            }),
        )?;
        conn.close_without_checkpoint()?;
        let wal = sidecar(&path, "-wal");
        ensure!(
            fs::metadata(wal)?.len() > 32,
            "fixture needs committed WAL rows"
        );
        Ok(path)
    }

    fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    }

    fn bundle(path: &Path) -> Result<SourceBundle> {
        ["", "-wal", "-shm"]
            .into_iter()
            .map(|suffix| {
                let path = sidecar(path, suffix);
                match fs::metadata(&path) {
                    Ok(metadata) => Ok(Some((fs::read(path)?, metadata.modified()?))),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error.into()),
                }
            })
            .collect()
    }

    #[test]
    fn standard_factory_discovers_native_database_and_reconstruction_wal() -> Result<()> {
        let home = tempfile::tempdir()?;
        let path = fixture(home.path(), "openclaw", "native", "nativeclawneedle")?;
        let before = bundle(&path)?;
        let sources = connector().discover_source_files(&context(home.path(), home.path()))?;
        assert_eq!(
            sources.iter().filter(|s| s.source_path == path).count(),
            1,
            "native database must be discovered exactly once"
        );
        assert!(
            sources.iter().any(|s| {
                s.source_path == path && s.role == DiscoveredSourceRole::SqliteDatabase
            })
        );
        assert!(sources.iter().any(|s| {
            s.source_path == sidecar(&path, "-wal")
                && s.role == DiscoveredSourceRole::MetadataSidecar
                && s.required_for_reconstruction
        }));
        assert_eq!(
            bundle(&path)?,
            before,
            "discovery mutated the provider store"
        );
        Ok(())
    }

    #[test]
    fn native_scan_preserves_order_workspace_identity_and_invocations() -> Result<()> {
        let home = tempfile::tempdir()?;
        let path = fixture(home.path(), "研究-agent", "session-🚀", "nativeclawneedle")?;
        let before = bundle(&path)?;
        let conversations = connector().scan(&context(home.path(), home.path()))?;
        assert_eq!(conversations.len(), 1);
        let conversation = &conversations[0];
        assert_eq!(conversation.agent_slug, "openclaw/研究-agent");
        assert_eq!(
            conversation.external_id.as_deref(),
            Some("研究-agent/session-🚀")
        );
        assert_eq!(conversation.source_path, path);
        assert_eq!(
            conversation.workspace.as_deref(),
            Some(Path::new("/work/native-history"))
        );
        assert_eq!(conversation.metadata["storage"], "sqlite");
        assert_eq!(conversation.metadata["session_id"], "session-🚀");
        assert_eq!(conversation.metadata["last_event_seq"], 5);
        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(conversation.messages[0].idx, 0);
        assert_eq!(conversation.messages[0].role, "user");
        assert_eq!(
            conversation.messages[0].content,
            "Historical OpenClaw transcript\nnativeclawneedle"
        );
        assert_eq!(conversation.messages[0].created_at, Some(AT + 1_000));
        assert_eq!(conversation.messages[1].idx, 1);
        assert!(conversation.messages[1].content.contains("東京 🚀"));
        let call = &conversation.messages[1].invocations[0];
        assert_eq!(call.name, "read_file");
        assert_eq!(call.call_id.as_deref(), Some("call-5"));
        assert_eq!(
            bundle(&path)?,
            before,
            "scan changed database/WAL bytes or mtimes"
        );
        Ok(())
    }

    #[test]
    fn mixed_migration_prefers_native_history_without_losing_legacy_only_sessions() -> Result<()> {
        let home = tempfile::tempdir()?;
        let database = fixture(home.path(), "openclaw", "shared", "nativeclawneedle")?;
        let sessions = home.path().join(".openclaw/agents/openclaw/sessions");
        fs::create_dir_all(&sessions)?;
        for id in ["shared", "legacy-only"] {
            fs::write(
                sessions.join(format!("{id}.jsonl")),
                format!(
                    "{}\n",
                    json!({
                        "type": "message", "message": {"role": "user", "content": format!("legacy {id}")}
                    })
                ),
            )?;
        }
        let conversations = connector().scan(&context(home.path(), home.path()))?;
        assert_eq!(conversations.len(), 2);
        let native = conversations
            .iter()
            .find(|c| c.external_id.as_deref() == Some("shared"))
            .unwrap();
        assert_eq!(native.source_path, database);
        assert_eq!(
            native.messages[0].content,
            "Historical OpenClaw transcript\nnativeclawneedle"
        );
        assert!(conversations.iter().any(|c| {
            c.external_id.as_deref() == Some("legacy-only")
                && c.messages[0].content == "legacy legacy-only"
        }));
        Ok(())
    }

    #[test]
    fn explicit_native_database_root_does_not_scan_a_sibling_agent() -> Result<()> {
        let home = tempfile::tempdir()?;
        let selected = fixture(home.path(), "openclaw", "selected", "selectedneedle")?;
        fixture(home.path(), "other", "hidden", "outside-scope")?;
        let conversations = connector().scan(&context(home.path(), &selected))?;
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].source_path, selected);
        assert_eq!(conversations[0].external_id.as_deref(), Some("selected"));
        Ok(())
    }

    fn command(home: &Path, data: &Path) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command
            .env_clear()
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("PATH", "")
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("APPDATA", home.join("AppData/Roaming"))
            .env("LOCALAPPDATA", home.join("AppData/Local"))
            .env("CASS_DATA_DIR", data)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("RUST_MIN_STACK", "134217728")
            .env("RAYON_NUM_THREADS", "2")
            .current_dir(home);
        if let Ok(system_root) = dotenvy::var("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command
    }

    #[test]
    fn standard_cli_indexes_and_retrieves_native_sqlite_history() -> Result<()> {
        let home = tempfile::tempdir()?;
        let database = fixture(home.path(), "openclaw", "native", "nativeclawneedle")?;
        let before = bundle(&database)?;
        let data = home.path().join("cass-data");
        assert_cmd::Command::from_std(command(home.path(), &data))
            .args(["index", "--full", "--json"])
            .timeout(Duration::from_secs(120))
            .assert()
            .success();
        let rows = canonical_messages(&data, &database)?;
        assert_eq!(rows.len(), 2);
        native_search_message(home.path(), &data, &database, "nativeclawneedle", &rows[0])?;
        assert_eq!(
            bundle(&database)?,
            before,
            "CLI mutated the native provider store"
        );
        Ok(())
    }

    fn search_hits(home: &Path, data: &Path, needle: &str) -> Result<Vec<SearchMessage>> {
        let output = assert_cmd::Command::from_std(command(home, data))
            .args([
                "search",
                needle,
                "--mode",
                "lexical",
                "--json",
                "--no-maintenance",
                "--limit",
                "10",
            ])
            .timeout(Duration::from_secs(30))
            .output()?;
        ensure!(
            output.status.success(),
            "search {needle} failed: {output:?}"
        );
        #[derive(Deserialize)]
        struct SearchOutput {
            hits: Vec<SearchMessage>,
        }
        let result: SearchOutput = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("search {needle} returned malformed hits: {output:?}"))?;
        Ok(result.hits)
    }

    fn single_search_message(home: &Path, data: &Path, needle: &str) -> Result<SearchMessage> {
        let hits = search_hits(home, data, needle)?;
        ensure!(
            hits.len() == 1,
            "unique needle {needle} must have exactly one hit: {hits:?}"
        );
        hits.into_iter().next().context("validated search hit")
    }

    fn native_search_message(
        home: &Path,
        data: &Path,
        database: &Path,
        needle: &str,
        expected: &CanonicalMessage,
    ) -> Result<SearchMessage> {
        let hit = single_search_message(home, data, needle)?;
        assert_eq!(hit.agent, "openclaw");
        assert_eq!(hit.source_id, "local");
        assert_eq!(hit.source_path, dunce::canonicalize(database)?);
        assert_eq!(hit.conversation_id, expected.conversation_id);
        assert_eq!(hit.line_number, usize::try_from(expected.idx)? + 1);
        assert_eq!(hit.created_at, expected.created_at);
        assert_eq!(hit.content, expected.content);
        Ok(hit)
    }

    fn canonical_messages(data: &Path, source: &Path) -> Result<Vec<CanonicalMessage>> {
        let storage = FrankenStorage::open_readonly(&data.join("agent_search.db"))?;
        let source = dunce::canonicalize(source)?;
        let rows = storage.raw().query_map_collect(
            "SELECT c.id, m.id, m.idx, m.created_at, m.content
             FROM conversations c JOIN messages m ON m.conversation_id = c.id
             WHERE c.source_id = 'local' AND c.source_path = ?1 ORDER BY m.id",
            &[ParamValue::from(source.to_string_lossy().as_ref())],
            |row| {
                Ok(CanonicalMessage {
                    conversation_id: row.get_typed(0)?,
                    message_id: row.get_typed(1)?,
                    idx: row.get_typed(2)?,
                    created_at: row.get_typed(3)?,
                    content: row.get_typed(4)?,
                })
            },
        )?;
        storage.close_without_checkpoint()?;
        let mut next_idx = BTreeMap::new();
        for row in &rows {
            let expected = next_idx.entry(row.conversation_id).or_insert(0);
            assert_eq!(row.idx, *expected, "canonical message order changed");
            *expected += 1;
        }
        Ok(rows)
    }

    fn canonical_append(
        data: &Path,
        database: &Path,
        previous: &[CanonicalMessage],
        conversation_id: i64,
        needle: &str,
        created_at: i64,
    ) -> Result<Vec<CanonicalMessage>> {
        let rows = canonical_messages(data, database)?;
        assert_eq!(rows.len(), previous.len() + 1);
        assert_eq!(&rows[..previous.len()], previous, "older history changed");
        let appended = rows.last().context("new canonical message")?;
        let prior_count = previous
            .iter()
            .filter(|row| row.conversation_id == conversation_id)
            .count();
        ensure!(prior_count > 0, "append must target an indexed session");
        assert_eq!(appended.conversation_id, conversation_id);
        assert_eq!(appended.idx, i64::try_from(prior_count)?);
        assert_eq!(appended.created_at, Some(created_at));
        assert_eq!(appended.content, needle);
        Ok(rows)
    }

    fn last_indexed_at(data: &Path) -> Result<i64> {
        let storage = FrankenStorage::open_readonly(&data.join("agent_search.db"))?;
        // The old timestamp shortcut also admitted sources with replay debt.
        // Prove this fixture exercises freshness admission, not recovery.
        let replay_debt: Vec<i64> = storage.raw().query_map_collect(
            "SELECT 1 FROM meta WHERE key >= ?1 AND key < ?2 LIMIT 1",
            &[
                ParamValue::from("watch_lexical_replay_v1:"),
                ParamValue::from("watch_lexical_replay_v1;"),
            ],
            |row| row.get_typed(0),
        )?;
        ensure!(
            replay_debt.is_empty(),
            "fixture must have no pending watch lexical replay debt"
        );
        let indexed_at = storage
            .get_last_indexed_at()?
            .context("successful indexing must record its completion")?;
        storage.close_without_checkpoint()?;
        Ok(indexed_at)
    }

    fn assert_source_mtimes_before(database: &Path, indexed_at: i64) -> Result<()> {
        for suffix in ["", "-wal"] {
            let path = sidecar(database, suffix);
            let modified_ms = fs::metadata(&path)?
                .modified()?
                .duration_since(UNIX_EPOCH)?
                .as_millis();
            ensure!(
                modified_ms <= u128::try_from(indexed_at)?,
                "fixture must reproduce the stale shortcut: {} mtime {modified_ms} exceeds archive completion {indexed_at}",
                path.display()
            );
        }
        Ok(())
    }

    fn watch_once(home: &Path, data: &Path, paths: &[&Path]) {
        let mut index = assert_cmd::Command::from_std(command(home, data));
        index.args(["index", "--watch-once"]);
        for path in paths {
            index.arg(path);
        }
        index
            .args(["--json", "--no-progress-events"])
            .timeout(Duration::from_secs(120))
            .assert()
            .success();
    }

    /// GH #502/#543: targeted watch-once must see a commit that lands only in
    /// the WAL while the provider's writer stays open, whether the database,
    /// its WAL alone, or both are named. An intervening successful
    /// index of another source must not certify this source as already read.
    #[test]
    fn watch_once_reads_wal_only_commits_while_the_writer_stays_open() -> Result<()> {
        let home = tempfile::tempdir()?;
        let database = fixture(home.path(), "openclaw", "live", "firstwalneedle")?;
        let unselected = fixture(home.path(), "unselected", "hidden", "neverselectedneedle")?;
        let unselected_before = bundle(&unselected)?;
        let wal = sidecar(&database, "-wal");
        let data = home.path().join("cass-data");
        // The provider's writer opens before cass indexes, so anything its open
        // does to the main file happens before the archive's last index.
        let writer = Connection::open(database.to_string_lossy().as_ref())?;
        writer.execute("PRAGMA wal_autocheckpoint = 0")?;
        let source_before = bundle(&database)?;
        watch_once(home.path(), &data, &[database.as_path()]);
        assert_eq!(bundle(&database)?, source_before);
        let mut previous = canonical_messages(&data, &database)?;
        assert_eq!(previous.len(), 2);
        assert_eq!(previous[0].idx, 0);
        assert_eq!(previous[1].idx, 1);
        assert_eq!(previous[0].created_at, Some(AT + 1_000));
        let seeded = native_search_message(
            home.path(),
            &data,
            &database,
            "firstwalneedle",
            &previous[0],
        )?;
        assert!(search_hits(home.path(), &data, "neverselectedneedle")?.is_empty());
        assert!(canonical_messages(&data, &unselected)?.is_empty());

        for (seq, needle, paths, interleave_source) in [
            (6, "secondwalneedle", vec![database.as_path()], false),
            (
                7,
                "thirdwalneedle",
                vec![database.as_path(), wal.as_path()],
                false,
            ),
            (8, "crosssourcewalneedle", vec![database.as_path()], true),
            (
                9,
                "crosssourcesidecarneedle",
                vec![database.as_path(), wal.as_path()],
                true,
            ),
            (10, "walonlyselectionneedle", vec![wal.as_path()], false),
            (
                11,
                "crosssourcewalonlyselectionneedle",
                vec![wal.as_path()],
                true,
            ),
        ] {
            // Keep the commit strictly after the last index on coarse clocks.
            std::thread::sleep(Duration::from_millis(1_100));
            let main_before = bundle(&database)?.swap_remove(0);
            append(
                &writer,
                "live",
                seq,
                json!({"type": "message", "message": {"role": "user", "content": needle}}),
            )?;
            ensure!(
                bundle(&database)?.swap_remove(0) == main_before,
                "the commit must land only in the WAL, leaving the main file untouched"
            );
            ensure!(
                search_hits(home.path(), &data, needle)?.is_empty(),
                "{needle} must not be searchable before the pass"
            );
            let source_before = bundle(&database)?;
            if interleave_source {
                let sessions = home.path().join(".openclaw/agents/openclaw/sessions");
                fs::create_dir_all(&sessions)?;
                let other = sessions.join(format!("other-{seq}.jsonl"));
                let other_needle = format!("othersourceneedle{seq}");
                fs::write(
                    &other,
                    format!(
                        "{}\n{}\n",
                        json!({"type": "session", "id": format!("other-{seq}"), "cwd": "/work/other"}),
                        json!({"type": "message", "message": {"role": "user", "content": other_needle}}),
                    ),
                )?;
                // This pass advances last_indexed_at after the WAL append,
                // without reading the explicitly unselected native database.
                let before_other = last_indexed_at(&data)?;
                watch_once(home.path(), &data, &[other.as_path()]);
                single_search_message(home.path(), &data, &other_needle)?;
                let after_other = last_indexed_at(&data)?;
                assert!(
                    after_other > before_other,
                    "the other source must advance the archive marker"
                );
                assert_source_mtimes_before(&database, after_other)?;
                ensure!(
                    search_hits(home.path(), &data, needle)?.is_empty(),
                    "the intervening source must not broaden to the native database"
                );
                assert_eq!(canonical_messages(&data, &database)?, previous);
            }
            watch_once(home.path(), &data, &paths);
            let rows = canonical_append(
                &data,
                &database,
                &previous,
                seeded.conversation_id,
                needle,
                AT + seq * 1_000,
            )?;
            let appended = native_search_message(
                home.path(),
                &data,
                &database,
                needle,
                rows.last().context("appended canonical message")?,
            )?;
            assert_eq!(
                single_search_message(home.path(), &data, "firstwalneedle")?,
                seeded,
                "historical search identity must survive later appends"
            );
            assert_eq!(
                bundle(&database)?,
                source_before,
                "targeted indexing must preserve provider database/WAL/SHM bytes and mtimes"
            );
            watch_once(home.path(), &data, &paths);
            assert_eq!(
                single_search_message(home.path(), &data, needle)?,
                appended,
                "re-reading must preserve search identity without duplicate hits"
            );
            assert_eq!(
                single_search_message(home.path(), &data, "firstwalneedle")?,
                seeded
            );
            assert_eq!(canonical_messages(&data, &database)?, rows);
            assert_eq!(bundle(&database)?, source_before);
            previous = rows;
        }
        assert!(search_hits(home.path(), &data, "neverselectedneedle")?.is_empty());
        assert!(canonical_messages(&data, &unselected)?.is_empty());
        assert_eq!(bundle(&unselected)?, unselected_before);
        writer.close_without_checkpoint()?;
        Ok(())
    }

    #[test]
    fn watch_once_reads_checkpointed_history_from_a_vanished_wal_path() -> Result<()> {
        let home = tempfile::tempdir()?;
        let database = fixture(
            home.path(),
            "openclaw",
            "checkpoint",
            "beforecheckpointneedle",
        )?;
        let wal = sidecar(&database, "-wal");
        let data = home.path().join("cass-data");
        let writer = Connection::open(database.to_string_lossy().as_ref())?;
        writer.execute("PRAGMA wal_autocheckpoint = 0")?;
        watch_once(home.path(), &data, &[database.as_path()]);
        let previous = canonical_messages(&data, &database)?;
        assert_eq!(previous.len(), 2);
        let seeded = native_search_message(
            home.path(),
            &data,
            &database,
            "beforecheckpointneedle",
            &previous[0],
        )?;
        let main_before = bundle(&database)?.swap_remove(0);
        append(
            &writer,
            "checkpoint",
            6,
            json!({"type": "message", "message": {"role": "user", "content": "checkpointedappendneedle"}}),
        )?;
        assert_eq!(bundle(&database)?.swap_remove(0), main_before);
        assert!(search_hits(home.path(), &data, "checkpointedappendneedle")?.is_empty());

        // A provider can checkpoint and remove its WAL after a hook records
        // the path. Reconstruct that durable state without a scheduling race.
        let checkpoint = writer.query("PRAGMA wal_checkpoint(TRUNCATE)")?;
        let result = checkpoint.first().context("provider checkpoint result")?;
        assert_eq!(
            [
                result.get_typed::<i64>(0)?,
                result.get_typed::<i64>(1)?,
                result.get_typed::<i64>(2)?,
            ],
            [0, 0, 0],
            "provider checkpoint must finish and drain every WAL frame"
        );
        writer.close_without_checkpoint()?;
        if wal.try_exists()? {
            ensure!(
                matches!(fs::metadata(&wal)?.len(), 0 | 32),
                "only a fully drained WAL may leave the provider path"
            );
            // FrankenSQLite may retain an empty WAL after close. Preserve it
            // as fixture evidence while making the recorded event path absent.
            fs::rename(&wal, sidecar(&database, "-wal.checkpointed-fixture"))?;
        }
        assert!(!wal.try_exists()?);
        let source_after_checkpoint = bundle(&database)?;
        assert_ne!(source_after_checkpoint[0], main_before);
        assert_eq!(canonical_messages(&data, &database)?, previous);

        watch_once(home.path(), &data, &[wal.as_path()]);
        let rows = canonical_append(
            &data,
            &database,
            &previous,
            seeded.conversation_id,
            "checkpointedappendneedle",
            AT + 6_000,
        )?;
        let appended = native_search_message(
            home.path(),
            &data,
            &database,
            "checkpointedappendneedle",
            rows.last().context("checkpointed canonical message")?,
        )?;
        assert_eq!(
            single_search_message(home.path(), &data, "beforecheckpointneedle")?,
            seeded
        );
        assert_eq!(bundle(&database)?, source_after_checkpoint);
        for paths in [vec![wal.as_path()], vec![database.as_path(), wal.as_path()]] {
            watch_once(home.path(), &data, &paths);
            assert_eq!(canonical_messages(&data, &database)?, rows);
            assert_eq!(
                single_search_message(home.path(), &data, "checkpointedappendneedle")?,
                appended
            );
            assert_eq!(
                single_search_message(home.path(), &data, "beforecheckpointneedle")?,
                seeded
            );
            assert_eq!(bundle(&database)?, source_after_checkpoint);
        }
        Ok(())
    }

    /// GH #543: reproduce the durable state left when a source commits after
    /// its snapshot was ingested but before that batch records completion.
    /// The completion marker is set deliberately; this is not a live race test.
    #[test]
    fn watch_once_reads_wal_commit_inside_previous_indexing_window() -> Result<()> {
        let home = tempfile::tempdir()?;
        let database = fixture(home.path(), "openclaw", "historical", "beforewindowneedle")?;
        let wal = sidecar(&database, "-wal");
        let data = home.path().join("cass-data");
        let writer = Connection::open(database.to_string_lossy().as_ref())?;
        writer.execute("PRAGMA wal_autocheckpoint = 0")?;
        append(
            &writer,
            "other-history",
            0,
            json!({"type": "session", "cwd": "/work/other-history"}),
        )?;
        append(
            &writer,
            "other-history",
            1,
            json!({"type": "message", "message": {"role": "user", "content": "otherhistoryneedle"}}),
        )?;
        let source_before = bundle(&database)?;
        watch_once(home.path(), &data, &[database.as_path()]);
        assert_eq!(bundle(&database)?, source_before);
        let previous = canonical_messages(&data, &database)?;
        assert_eq!(previous.len(), 3);
        let seeded_row = previous
            .iter()
            .find(|row| row.content.contains("beforewindowneedle"))
            .context("seeded historical message")?;
        assert_eq!(seeded_row.created_at, Some(AT + 1_000));
        let seeded = native_search_message(
            home.path(),
            &data,
            &database,
            "beforewindowneedle",
            seeded_row,
        )?;
        let other_history = native_search_message(
            home.path(),
            &data,
            &database,
            "otherhistoryneedle",
            previous
                .iter()
                .find(|row| row.content == "otherhistoryneedle")
                .context("second historical session")?,
        )?;
        assert_ne!(seeded.conversation_id, other_history.conversation_id);
        let snapshot_indexed_at = last_indexed_at(&data)?;
        // The real append follows the completed snapshot. Only the archive's
        // completion marker is reconstructed below; no timing race is required.
        append(
            &writer,
            "historical",
            6,
            json!({"type": "message", "message": {"role": "user", "content": "insidewindowneedle"}}),
        )?;
        let source_after_append = bundle(&database)?;
        assert_eq!(
            source_after_append[0], source_before[0],
            "commit must be WAL-only"
        );
        let wal_written_at = i64::try_from(
            fs::metadata(&wal)?
                .modified()?
                .duration_since(UNIX_EPOCH)?
                .as_millis(),
        )?;
        let batch_finished_at = wal_written_at
            .max(snapshot_indexed_at)
            .max(FrankenStorage::now_millis())
            + 10_000;
        {
            let storage = FrankenStorage::open_writer(&data.join("agent_search.db"))?;
            storage.set_last_indexed_at(batch_finished_at)?;
            storage.close_without_checkpoint()?;
        }
        assert_eq!(last_indexed_at(&data)?, batch_finished_at);
        assert_source_mtimes_before(&database, batch_finished_at)?;
        assert!(
            AT + 6_000 < snapshot_indexed_at,
            "the appended event is historical"
        );
        assert!(search_hits(home.path(), &data, "insidewindowneedle")?.is_empty());
        assert_eq!(canonical_messages(&data, &database)?, previous);

        watch_once(home.path(), &data, &[database.as_path()]);
        let rows = canonical_append(
            &data,
            &database,
            &previous,
            seeded.conversation_id,
            "insidewindowneedle",
            AT + 6_000,
        )?;
        let appended = native_search_message(
            home.path(),
            &data,
            &database,
            "insidewindowneedle",
            rows.last().context("appended historical message")?,
        )?;
        assert_eq!(bundle(&database)?, source_after_append);
        assert_eq!(
            single_search_message(home.path(), &data, "beforewindowneedle")?,
            seeded
        );
        assert_eq!(
            single_search_message(home.path(), &data, "otherhistoryneedle")?,
            other_history
        );
        for paths in [
            vec![database.as_path()],
            vec![database.as_path(), wal.as_path()],
        ] {
            watch_once(home.path(), &data, &paths);
            assert_eq!(
                single_search_message(home.path(), &data, "insidewindowneedle")?,
                appended
            );
            assert_eq!(
                single_search_message(home.path(), &data, "beforewindowneedle")?,
                seeded
            );
            assert_eq!(
                single_search_message(home.path(), &data, "otherhistoryneedle")?,
                other_history
            );
            assert_eq!(canonical_messages(&data, &database)?, rows);
            assert_eq!(bundle(&database)?, source_after_append);
        }
        writer.close_without_checkpoint()?;
        Ok(())
    }
}
