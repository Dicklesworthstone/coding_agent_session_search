//! First-class Oh My Pi v18 integration gates.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context as _;
use coding_agent_search::connectors::{
    Connector, Origin, Platform, ScanContext, ScanRoot, extract_tokens_for_agent,
    get_connector_factories, omp::OmpConnector,
};
use coding_agent_search::sources::sync::path_to_safe_dirname;
use serde_json::json;

const OMP_LIVE_OVERRIDE_CHILD_ENV: &str = "CASS_TEST_OMP_LIVE_OVERRIDE_CHILD";
const OMP_LIVE_OVERRIDE_STATE_DIR_ENV: &str = "CASS_TEST_OMP_LIVE_OVERRIDE_STATE_DIR";
const OMP_LIVE_OVERRIDE_CHILD_RECEIPT: &str = "cass-omp-live-override-child-ok";

fn write_omp_session(agent_root: &Path, id: &str, title: &str) -> PathBuf {
    let session_dir = agent_root.join("sessions/-projects-cass");
    fs::create_dir_all(&session_dir).expect("create OMP session directory");
    let path = session_dir.join(format!("2026-08-23T12-00-00_{id}.jsonl"));
    let transcript = [
        json!({"type":"title","title":title}),
        json!({"type":"session","version":3,"id":id,"timestamp":"2026-08-23T12:00:00Z","cwd":"/projects/cass"}),
        json!({"type":"model_change","timestamp":"2026-08-23T12:00:01Z","model":"openrouter/stealth/ox-alpha"}),
        json!({"type":"message","timestamp":"2026-08-23T12:00:02Z","message":{"role":"user","content":"index OMP"}}),
        json!({"type":"message","timestamp":"2026-08-23T12:00:03Z","message":{"role":"assistant","model":"openrouter/stealth/ox-alpha","content":"done"}}),
    ]
    .into_iter()
    .map(|entry| entry.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    fs::write(&path, format!("{transcript}\n")).expect("write OMP session");
    path
}

fn write_omp_subagent(main_session: &Path, id: &str) -> PathBuf {
    let subagent_dir = main_session.with_extension("");
    fs::create_dir_all(&subagent_dir).expect("create OMP sub-agent directory");
    let path = subagent_dir.join("Researcher.jsonl");
    let transcript = [
        json!({"type":"session","version":3,"id":id,"timestamp":"2026-08-23T12:01:00Z","cwd":"/projects/cass"}),
        json!({"type":"model_change","timestamp":"2026-08-23T12:01:01Z","model":"openrouter/stealth/ox-alpha"}),
        json!({"type":"message","timestamp":"2026-08-23T12:01:02Z","message":{"role":"user","content":"OMP sub-agent task"}}),
        json!({"type":"message","timestamp":"2026-08-23T12:01:03Z","message":{"role":"assistant","model":"openrouter/stealth/ox-alpha","content":"sub-agent done"}}),
    ]
    .into_iter()
    .map(|entry| entry.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    fs::write(&path, format!("{transcript}\n")).expect("write OMP sub-agent session");
    path
}

fn runtime_connector(name: &str) -> Box<dyn Connector + Send> {
    get_connector_factories()
        .into_iter()
        .find_map(|(slug, factory)| (slug == name).then_some(factory))
        .unwrap_or_else(|| panic!("missing runtime connector factory for {name}"))()
}

#[test]
fn gh424_deferred_omp_analytics_cli_maintenance_preserves_published_search() -> anyhow::Result<()> {
    use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
    use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use coding_agent_search::storage::sqlite::SqliteStorage;
    use fs2::FileExt;
    use serde_json::Value;
    use std::time::Duration;

    let temp = tempfile::tempdir()?;
    let home = temp.path();
    let data_dir = home.join("cass state");
    fs::create_dir_all(&data_dir)?;
    let db_path = home.join("custom archive.db");
    // A historical canonical archive fixture, not a claim of live application history.
    let storage = SqliteStorage::open(&db_path)?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "pi_agent".into(),
        name: "Pi Agent".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let conversation = Conversation {
        id: None,
        agent_slug: "pi_agent".into(),
        workspace: None,
        external_id: Some("legacy-omp-maintenance".into()),
        title: Some("OMP repair".into()),
        source_path: home.join(".omp/agent/sessions/work/legacy.jsonl"),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_001_000),
        approx_tokens: None,
        metadata_json: json!({"source":"pi_agent"}),
        source_id: "local".into(),
        origin_host: None,
        messages: (0..2)
            .map(|idx| Message {
                id: None,
                idx,
                role: if idx == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Agent
                },
                author: None,
                created_at: Some(1_700_000_000_000 + idx * 1_000),
                content: format!("omprepairproofz original message {idx}"),
                extra_json: json!({}),
                snippets: vec![],
            })
            .collect(),
    };
    storage.insert_conversations_batched(&[(agent, None, &conversation)])?;
    storage.rebuild_analytics()?;
    storage.rebuild_token_daily_stats()?;
    storage.rebuild_daily_stats()?;
    let ledger_totals = |storage: &SqliteStorage| -> anyhow::Result<(i64, i64, i64, i64)> {
        Ok(storage.raw().query_row_map("SELECT COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0), COALESCE(SUM(total_tokens), 0) FROM token_usage", &[],
            |row| Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?, row.get_typed(3)?)))?)
    };
    let token_rollup_totals = |storage: &SqliteStorage| -> anyhow::Result<(i64, i64, i64, i64)> {
        Ok(storage.raw().query_row_map("SELECT api_call_count, total_input_tokens, total_output_tokens, grand_total_tokens FROM token_daily_stats WHERE agent_slug = 'all' AND source_id = 'all' AND model_family = 'all'", &[],
            |row| Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?, row.get_typed(3)?)))?)
    };
    let daily_totals = |storage: &SqliteStorage| -> anyhow::Result<(i64, i64, i64)> {
        Ok(storage.raw().query_row_map("SELECT session_count, message_count, total_chars FROM daily_stats WHERE agent_slug = 'all' AND source_id = 'all'", &[],
            |row| Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?)))?)
    };
    // These are the actual persisted estimates, not an API-usage parsing oracle.
    let original_ledger = ledger_totals(&storage)?;
    assert_eq!(original_ledger.0, 2);
    assert!(original_ledger.3 > 0);
    let original_token_rollup = token_rollup_totals(&storage)?;
    assert_eq!(original_token_rollup, original_ledger);
    let original_daily = daily_totals(&storage)?;
    assert_eq!((original_daily.0, original_daily.1), (1, 2));
    assert!(original_daily.2 > 0);
    storage.raw().execute("INSERT INTO meta(key, value) VALUES('legacy_omp_reclassification_v1', 'analytics_pending')")?;
    let original_id = storage.list_conversations(10, 0)?[0].id.unwrap();
    let original_ids: Vec<_> = storage
        .fetch_messages(original_id)?
        .iter()
        .map(|m| m.id)
        .collect();
    storage.close()?;

    let command = |args: &[&str], defer: bool| -> Command {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        cmd.env_clear()
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("CASS_DATA_DIR", &data_dir)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .env("RUST_MIN_STACK", "134217728")
            .env(
                "CASS_DEFER_ANALYTICS_UPDATES",
                if defer { "1" } else { "0" },
            )
            .current_dir(home)
            .arg("--db")
            .arg(&db_path)
            .args(args)
            .arg("--data-dir")
            .arg(&data_dir);
        cmd
    };
    let parse_output =
        |args: &[&str], output: &std::process::Output, expected_success: bool| -> Value {
            let diagnostic = format!(
                "args={args:?}; status={}; stdout={}; stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.status.success(), expected_success, "{diagnostic}");
            if expected_success {
                serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                    panic!("invalid success stdout JSON: {error}; {diagnostic}")
                })
            } else {
                assert!(
                    output.stdout.is_empty(),
                    "failure emitted stdout; {diagnostic}"
                );
                // main::handle_fatal_error emits the terminal robot envelope on
                // stderr, after any analytics progress diagnostics.
                let stderr = String::from_utf8_lossy(&output.stderr);
                let terminal = stderr
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or_else(|| panic!("missing failure stderr JSON; {diagnostic}"));
                let envelope: Value = serde_json::from_str(terminal).unwrap_or_else(|error| {
                    panic!("invalid failure stderr JSON: {error}; {diagnostic}")
                });
                assert_eq!(
                    envelope["error"]["code"].as_i64(),
                    output.status.code().map(i64::from),
                    "error envelope and process exit disagree; {diagnostic}"
                );
                envelope
            }
        };
    let run = |args: &[&str], defer: bool, expected_success: bool| -> Value {
        let output = assert_cmd::Command::from_std(command(args, defer))
            .timeout(Duration::from_secs(120))
            .output()
            .unwrap_or_else(|error| panic!("CLI failed to complete: {error}; args={args:?}"));
        parse_output(args, &output, expected_success)
    };
    let refused = run(
        &["analytics", "rebuild", "--track", "all", "--json"],
        false,
        false,
    );
    assert_eq!(refused["error"]["code"], 9, "{refused}");
    assert_eq!(refused["error"]["kind"], "rebuild-error", "{refused}");
    assert_eq!(refused["error"]["retryable"], true, "{refused}");
    let refusal = refused.to_string();
    assert!(
        refusal.contains("cannot rebuild legacy OMP analytics without a migration marker"),
        "{refused}"
    );
    assert!(
        refusal.contains("custom archive.db") && refusal.contains("cass state"),
        "{refused}"
    );
    let before_index = SqliteStorage::open_readonly(&db_path)?;
    let legacy_state: String = before_index.raw().query_row_map(
        "SELECT value FROM meta WHERE key = 'legacy_omp_reclassification_v1'",
        &[],
        |row| row.get_typed(0),
    )?;
    assert_eq!(legacy_state, "analytics_pending");
    let v2_rows: i64 = before_index.raw().query_row_map(
        "SELECT COUNT(*) FROM meta WHERE key = 'legacy_omp_reclassification_v2'",
        &[],
        |row| row.get_typed(0),
    )?;
    assert_eq!(v2_rows, 0);
    assert_eq!(
        before_index.list_conversations(10, 0)?[0].agent_slug,
        "pi_agent"
    );
    assert_eq!(
        before_index
            .fetch_messages(original_id)?
            .iter()
            .map(|m| m.id)
            .collect::<Vec<_>>(),
        original_ids
    );
    let metrics_before_index: i64 = before_index.raw().query_row_map(
        "SELECT COUNT(*) FROM message_metrics WHERE agent_slug = 'pi_agent'",
        &[],
        |row| row.get_typed(0),
    )?;
    assert_eq!(metrics_before_index, 2);
    assert_eq!(ledger_totals(&before_index)?, original_ledger);
    assert_eq!(token_rollup_totals(&before_index)?, original_token_rollup);
    assert_eq!(daily_totals(&before_index)?, original_daily);
    before_index.close_without_checkpoint()?;
    run(&["index", "--full", "--json"], true, true);
    let lexical_path = coding_agent_search::search::tantivy::expected_index_dir(&data_dir);
    let manifest = fs::read(lexical_path.join("MANIFEST"))?;
    let search = || {
        let result = run(
            &[
                "search",
                "omprepairproofz",
                "--agent",
                "omp",
                "--mode",
                "lexical",
                "--json",
                "--no-maintenance",
                "--limit",
                "10",
            ],
            false,
            true,
        );
        assert_eq!(result["hits"].as_array().unwrap().len(), 2, "{result}");
    };
    search();
    let assert_pending = || {
        let status = run(&["analytics", "status", "--json"], false, true);
        assert_eq!(
            status["data"]["recommended_action"], "rebuild_all",
            "{status}"
        );
        let signal = status["data"]["drift"]["signals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|signal| signal["signal"] == "legacy_omp_analytics_pending")
            .unwrap();
        assert!(
            signal["detail"]
                .as_str()
                .unwrap()
                .contains("custom archive.db")
        );
        let validation = run(&["analytics", "validate", "--json"], false, true);
        let check = validation["data"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["id"] == "migration.legacy_omp_analytics_pending")
            .unwrap();
        assert_eq!(check["ok"], false);
        assert!(
            check["suggested_action"]
                .as_str()
                .unwrap()
                .contains("custom archive.db")
        );
    };
    assert_pending();
    // A single-track rebuild can make every Track A bucket agree while the
    // all-projection migration authority must remain pending.
    run(
        &["analytics", "rebuild", "--track", "a", "--json"],
        false,
        true,
    );
    assert_pending();
    let scoped = run(
        &[
            "analytics",
            "rebuild",
            "--track",
            "all",
            "--since",
            "2023-11-14",
            "--json",
        ],
        false,
        true,
    );
    assert!(scoped["data"]["since_ms"].is_i64(), "{scoped}");
    assert_pending();
    let doctor = run(&["doctor", "--json"], false, true);
    let check = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "legacy_omp_analytics")
        .unwrap();
    assert_eq!(check["status"], "warn");
    assert_eq!(check["fix_applied"], false);
    assert!(
        check["message"]
            .as_str()
            .unwrap()
            .contains("custom archive.db")
    );

    // A real advisory lock makes the conflict deterministic. No child is
    // allowed to open the writer, migrate schema, or reset any projection.
    let held_lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data_dir.join("index-run.lock"))?;
    held_lock.try_lock_exclusive()?;
    let db_before_busy = fs::read(&db_path)?;
    let wal_path = db_path.with_extension("db-wal");
    let wal_before_busy = if wal_path.exists() {
        Some(fs::read(&wal_path)?)
    } else {
        None
    };
    for args in [
        vec!["analytics", "rebuild", "--track", "all", "--json"],
        vec!["analytics", "rebuild", "--track", "a", "--json"],
        vec!["analytics", "rebuild", "--track", "b", "--json"],
        vec![
            "analytics",
            "rebuild",
            "--track",
            "all",
            "--since",
            "2023-11-14",
            "--json",
        ],
        vec!["analytics", "validate", "--fix", "--json"],
    ] {
        let output = assert_cmd::Command::from_std(command(&args, false))
            .timeout(Duration::from_secs(30))
            .assert()
            .code(7)
            .get_output()
            .clone();
        let error = parse_output(&args, &output, false);
        assert_eq!(error["error"]["code"], 7, "{error}");
        assert_eq!(error["error"]["kind"], "index-busy", "{error}");
        assert_eq!(error["error"]["retryable"], true, "{error}");
        assert_eq!(fs::read(&db_path)?, db_before_busy);
        let wal_after_busy = if wal_path.exists() {
            Some(fs::read(&wal_path)?)
        } else {
            None
        };
        assert_eq!(wal_after_busy, wal_before_busy);
    }
    // Read-only validation remains usable while another maintenance owner holds the lock.
    run(&["analytics", "validate", "--json"], false, true);
    assert_eq!(fs::read(&db_path)?, db_before_busy);
    let wal_after_read = if wal_path.exists() {
        Some(fs::read(&wal_path)?)
    } else {
        None
    };
    assert_eq!(wal_after_read, wal_before_busy);
    drop(held_lock);
    assert_pending();
    let rebuilt = run(
        &["analytics", "rebuild", "--track", "all", "--json"],
        false,
        true,
    );
    assert_eq!(rebuilt["data"]["tracks_rebuilt"], json!(["a", "b"]));
    assert_eq!(rebuilt["data"]["track_a"]["message_metrics_rows"], 2);
    let storage = SqliteStorage::open_readonly(&db_path)?;
    assert_eq!(storage.list_conversations(10, 0)?.len(), 1);
    let canonical = &storage.list_conversations(10, 0)?[0];
    assert_eq!(canonical.id, Some(original_id));
    assert_eq!(canonical.agent_slug, "omp");
    assert_eq!(
        storage
            .fetch_messages(original_id)?
            .iter()
            .map(|m| m.id)
            .collect::<Vec<_>>(),
        original_ids
    );
    let state: String = storage.raw().query_row_map(
        "SELECT value FROM meta WHERE key = 'legacy_omp_reclassification_v2'",
        &[],
        |row| row.get_typed(0),
    )?;
    assert!(state.starts_with("complete:"), "{state}");
    let metric_count: i64 = storage.raw().query_row_map(
        "SELECT COUNT(*) FROM message_metrics WHERE agent_slug = 'omp'",
        &[],
        |row| row.get_typed(0),
    )?;
    assert_eq!(metric_count, 2);
    let daily_messages: i64 =
        storage
            .raw()
            .query_row_map("SELECT SUM(message_count) FROM usage_daily", &[], |row| {
                row.get_typed(0)
            })?;
    assert_eq!(daily_messages, 2);
    let stale_metrics: i64 = storage.raw().query_row_map(
        "SELECT COUNT(*) FROM message_metrics WHERE agent_slug = 'pi_agent'",
        &[],
        |row| row.get_typed(0),
    )?;
    assert_eq!(stale_metrics, 0);
    assert_eq!(ledger_totals(&storage)?, original_ledger);
    assert_eq!(token_rollup_totals(&storage)?, original_token_rollup);
    assert_eq!(daily_totals(&storage)?, original_daily);
    let omp_ledger_rows: i64 = storage.raw().query_row_map("SELECT COUNT(*) FROM token_usage t JOIN agents a ON a.id = t.agent_id WHERE a.slug = 'omp'", &[], |row| row.get_typed(0))?;
    assert_eq!(omp_ledger_rows, original_ledger.0);
    let stale_token_rollups: i64 = storage.raw().query_row_map(
        "SELECT COUNT(*) FROM token_daily_stats WHERE agent_slug = 'pi_agent'",
        &[],
        |row| row.get_typed(0),
    )?;
    assert_eq!(stale_token_rollups, 0);
    let stale_daily_rows: i64 = storage.raw().query_row_map(
        "SELECT COUNT(*) FROM daily_stats WHERE agent_slug = 'pi_agent'",
        &[],
        |row| row.get_typed(0),
    )?;
    assert_eq!(stale_daily_rows, 0);
    storage.close_without_checkpoint()?;
    let status = run(&["analytics", "status", "--json"], false, true);
    assert!(
        !status["data"]["drift"]["signals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|signal| signal["signal"] == "legacy_omp_analytics_pending")
    );
    search();
    assert_eq!(fs::read(lexical_path.join("MANIFEST"))?, manifest);
    run(
        &["analytics", "rebuild", "--track", "all", "--json"],
        false,
        true,
    );
    run(&["analytics", "validate", "--fix", "--json"], false, true);
    let replay = SqliteStorage::open_readonly(&db_path)?;
    assert_eq!(replay.list_conversations(10, 0)?.len(), 1);
    assert_eq!(
        replay
            .fetch_messages(original_id)?
            .iter()
            .map(|m| m.id)
            .collect::<Vec<_>>(),
        original_ids
    );
    let replay_daily_messages: i64 =
        replay
            .raw()
            .query_row_map("SELECT SUM(message_count) FROM usage_daily", &[], |row| {
                row.get_typed(0)
            })?;
    assert_eq!(replay_daily_messages, 2);
    assert_eq!(ledger_totals(&replay)?, original_ledger);
    assert_eq!(token_rollup_totals(&replay)?, original_token_rollup);
    assert_eq!(daily_totals(&replay)?, original_daily);
    assert_eq!(fs::read(lexical_path.join("MANIFEST"))?, manifest);
    Ok(())
}

#[test]
fn omp_v18_profiles_are_first_class_and_not_scanned_by_pi_agent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("copied-home");
    let default_agent = home.join(".omp/agent");
    let profile_agent = home.join(".omp/profiles/work/agent");
    let default_session = write_omp_session(&default_agent, "omp-default", "Default OMP session");
    write_omp_subagent(&default_session, "omp-default-researcher");
    write_omp_session(&profile_agent, "omp-work", "Profile OMP session");

    let ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::local(home.clone())],
        None,
    );
    let mut conversations = runtime_connector("omp")
        .scan(&ctx)
        .expect("scan OMP fixtures through the production registry");
    conversations.sort_by(|left, right| left.external_id.cmp(&right.external_id));

    assert_eq!(conversations.len(), 3);
    for conversation in &conversations {
        assert_eq!(conversation.agent_slug, "omp");
        assert_eq!(conversation.metadata["source"], "omp");
        assert_eq!(
            conversation.metadata["model_id"],
            "openrouter/stealth/ox-alpha"
        );
    }
    let profile = conversations
        .iter()
        .find(|conversation| conversation.title.as_deref() == Some("Profile OMP session"))
        .expect("profile conversation");
    assert_eq!(profile.metadata["profile"], "work");
    assert!(
        conversations.iter().any(|conversation| {
            conversation.source_path.ends_with("Researcher.jsonl")
                && conversation
                    .messages
                    .iter()
                    .any(|message| message.content == "OMP sub-agent task")
        }),
        "OMP sub-agent transcripts must remain independently searchable"
    );

    let pi_conversations = runtime_connector("pi_agent")
        .scan(&ctx)
        .expect("scan Pi Agent through the production registry against the same copied home");
    assert!(
        pi_conversations.is_empty(),
        "the dedicated Pi Agent connector must not duplicate OMP sessions"
    );
}

#[test]
fn omp_direct_profile_root_preserves_profile_metadata() {
    let temp = tempfile::tempdir().expect("tempdir");
    let profile_agent = temp.path().join(".omp/profiles/review/agent");
    write_omp_session(&profile_agent, "omp-review", "Profile root OMP session");
    let ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::local(profile_agent.join("sessions"))],
        None,
    );

    let conversations = runtime_connector("omp")
        .scan(&ctx)
        .expect("scan a direct OMP profile root through the production registry");

    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].agent_slug, "omp");
    assert_eq!(conversations[0].metadata["profile"], "review");
}

#[test]
fn direct_pi_sessions_root_is_never_parsed_as_omp() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pi_agent = temp.path().join(".pi/agent");
    write_omp_session(&pi_agent, "pi-only", "Pi-only session");
    let ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::local(pi_agent.join("sessions"))],
        None,
    );

    let pi_conversations = runtime_connector("pi_agent")
        .scan(&ctx)
        .expect("scan direct Pi sessions root");
    let omp_conversations = runtime_connector("omp")
        .scan(&ctx)
        .expect("apply OMP ownership boundary to direct Pi sessions root");

    assert_eq!(pi_conversations.len(), 1);
    assert_eq!(pi_conversations[0].agent_slug, "pi_agent");
    assert!(
        omp_conversations.is_empty(),
        "a basename of `sessions` alone must not make a canonical Pi store OMP"
    );
}

#[test]
fn explicit_xdg_omp_root_is_never_parsed_as_pi_agent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let xdg_app = temp.path().join(".local/share/omp");
    write_omp_session(&xdg_app, "omp-xdg", "XDG OMP session");
    let ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::local(xdg_app)],
        None,
    );

    let omp_conversations = runtime_connector("omp")
        .scan(&ctx)
        .expect("scan explicit OMP XDG root");
    let pi_conversations = runtime_connector("pi_agent")
        .scan(&ctx)
        .expect("apply Pi ownership boundary to OMP XDG root");

    assert_eq!(omp_conversations.len(), 1);
    assert_eq!(omp_conversations[0].agent_slug, "omp");
    assert!(
        pi_conversations.is_empty(),
        "the shared pi-family wire format must not let Pi duplicate an XDG OMP session"
    );
}

#[test]
fn sanitized_remote_omp_roots_keep_provider_identity() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mirror = temp.path().join("cass/remotes/build-host/mirror");
    fs::create_dir_all(&mirror).expect("create production-shaped mirror root");

    let cases = [
        (
            "~/.omp/agent/sessions",
            false,
            "omp-sanitized-default-tilde",
            "Sanitized default tilde mirror",
        ),
        (
            "/home/dev/.omp/agent/sessions",
            false,
            "omp-sanitized-default-absolute",
            "Sanitized default absolute mirror",
        ),
        (
            "~/.local/share/omp",
            true,
            "omp-sanitized-xdg-tilde",
            "Sanitized XDG tilde mirror",
        ),
        (
            "/home/dev/.local/share/omp",
            true,
            "omp-sanitized-xdg-absolute",
            "Sanitized XDG absolute mirror",
        ),
    ];

    for (remote_path, includes_leaf_dir, id, title) in cases {
        let root = mirror.join(path_to_safe_dirname(remote_path));
        let store_root = if includes_leaf_dir {
            root.join("omp")
        } else {
            root.clone()
        };
        write_omp_session(&store_root, id, title);
        let ctx = ScanContext::with_roots(
            temp.path().join("cass-state"),
            vec![ScanRoot::remote(
                root,
                Origin::remote_with_host("build-host", "build-host.example"),
                Some(Platform::Linux),
            )],
            None,
        );
        let omp_conversations = runtime_connector("omp")
            .scan(&ctx)
            .expect("scan sanitized OMP mirror root");
        let pi_conversations = runtime_connector("pi_agent")
            .scan(&ctx)
            .expect("apply Pi boundary to sanitized OMP mirror root");

        assert_eq!(omp_conversations.len(), 1);
        assert_eq!(omp_conversations[0].title.as_deref(), Some(title));
        assert!(pi_conversations.is_empty());
    }

    for (remote_path, id) in [
        ("~/.omp/agent/sessions", "tilde-lookalike"),
        ("/home/dev/.omp/agent/sessions", "absolute-lookalike"),
    ] {
        let safe_name = path_to_safe_dirname(remote_path);
        let non_mirror_root = temp.path().join("ordinary-cache").join(safe_name);
        write_omp_session(&non_mirror_root, id, "Non-mirror sanitized lookalike");
        let non_mirror_ctx = ScanContext::with_roots(
            temp.path().join("cass-state"),
            vec![ScanRoot::local(non_mirror_root)],
            None,
        );
        assert!(
            runtime_connector("omp")
                .scan(&non_mirror_ctx)
                .expect("apply OMP ownership boundary to non-mirror lookalike")
                .is_empty(),
            "a sanitized marker outside remotes/<source>/mirror must not claim OMP ownership"
        );
    }
}

#[test]
fn sanitized_remote_omp_profile_root_preserves_profile_subagents_and_provenance()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let mirror = temp.path().join("cass/remotes/build-host/mirror");
    let root = mirror.join(path_to_safe_dirname("~/.omp/profiles"));
    let profile_agent = root.join("work/agent");
    let main_session = write_omp_session(
        &profile_agent,
        "omp-mirrored-profile",
        "Mirrored profile session",
    );
    write_omp_subagent(&main_session, "omp-mirrored-profile-researcher");

    let ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::remote(
            root,
            Origin::remote_with_host("build-host", "build-host.example"),
            Some(Platform::Linux),
        )],
        None,
    );
    let connector = OmpConnector::new();
    let sources = connector.discover_source_files(&ctx)?;
    anyhow::ensure!(
        sources.len().cmp(&2).is_eq(),
        "expected the mirrored OMP profile and its sub-agent"
    );
    anyhow::ensure!(sources.iter().all(|source| {
        source.provider_slug.eq("omp")
            && source.origin.is_remote()
            && matches!(source.platform, Some(Platform::Linux))
    }));

    let conversations = connector.scan(&ctx)?;
    anyhow::ensure!(
        conversations.len().cmp(&2).is_eq(),
        "expected two indexed OMP conversations"
    );
    anyhow::ensure!(conversations.iter().all(|conversation| {
        conversation.agent_slug.eq("omp")
            && conversation
                .metadata
                .get("profile")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|profile| profile.eq("work"))
    }));
    anyhow::ensure!(conversations.iter().any(|conversation| {
        conversation.source_path.ends_with("Researcher.jsonl")
            && conversation
                .messages
                .iter()
                .any(|message| message.content.eq("OMP sub-agent task"))
    }));

    let resume = std::process::Command::new(env!("CARGO_BIN_EXE_cass"))
        .arg("resume")
        .arg(&main_session)
        .arg("--json")
        .output()?;
    anyhow::ensure!(
        resume.status.success(),
        "cass resume failed: {}",
        String::from_utf8_lossy(&resume.stderr)
    );
    let resume_payload: serde_json::Value = serde_json::from_slice(&resume.stdout)?;
    let resume_command = resume_payload
        .get("command")
        .context("cass resume response omitted command")?;
    let expected_command = json!([
        "omp",
        "--profile",
        "work",
        "--session-dir",
        profile_agent.join("sessions").display().to_string(),
        "--resume",
        "omp-mirrored-profile"
    ]);
    anyhow::ensure!(
        resume_command.eq(&expected_command),
        "resume must preserve the mirrored profile identity and exact session store"
    );

    let pi_conversations = runtime_connector("pi_agent").scan(&ctx)?;
    anyhow::ensure!(
        pi_conversations.is_empty(),
        "a mirrored OMP profile and its sub-agent must each index once"
    );

    Ok(())
}

#[test]
fn cass_omp_data_root_is_an_omp_only_live_override() {
    let temp = tempfile::tempdir().expect("tempdir");
    let omp_root = temp.path().join("custom-omp-store");
    let shared_pi_root = temp.path().join("shared-agent");
    write_omp_session(&omp_root, "omp-only-live-root", "OMP-only override");
    write_omp_session(&shared_pi_root, "shared-pi-root", "Shared Pi override");

    let output = Command::new(std::env::current_exe().expect("current connector_omp test binary"))
        .arg("--exact")
        .arg("cass_omp_data_root_is_an_omp_only_live_override_child")
        .arg("--nocapture")
        .env_clear()
        .env(OMP_LIVE_OVERRIDE_CHILD_ENV, "1")
        .env(OMP_LIVE_OVERRIDE_STATE_DIR_ENV, temp.path())
        .env("CASS_OMP_DATA_ROOT", &omp_root)
        .env("PI_CODING_AGENT_DIR", &shared_pi_root)
        .env("RUST_MIN_STACK", "16777216")
        .output()
        .expect("spawn isolated OMP live-override test child");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "isolated OMP live-override child failed\nstatus={:?}\n{combined}",
        output.status
    );
    assert!(
        combined.len() <= 1024 * 1024,
        "isolated OMP live-override child exceeded the 1 MiB diagnostic cap"
    );
    assert!(
        combined.contains(OMP_LIVE_OVERRIDE_CHILD_RECEIPT),
        "isolated OMP live-override child omitted its success receipt: {combined}"
    );
}

#[test]
fn cass_omp_data_root_is_an_omp_only_live_override_child() {
    let Some(child_marker) = std::env::var_os(OMP_LIVE_OVERRIDE_CHILD_ENV) else {
        return;
    };
    if child_marker.to_str() != Some("1") {
        return;
    }

    let state_dir = PathBuf::from(
        std::env::var_os(OMP_LIVE_OVERRIDE_STATE_DIR_ENV)
            .expect("OMP live-override child state directory"),
    );
    let omp_root = PathBuf::from(
        std::env::var_os("CASS_OMP_DATA_ROOT").expect("OMP live-override child OMP root"),
    );
    let shared_pi_root = PathBuf::from(
        std::env::var_os("PI_CODING_AGENT_DIR").expect("OMP live-override child shared Pi root"),
    );

    let detection = runtime_connector("omp").detect();
    assert!(
        detection.root_paths.iter().any(|root| root == &omp_root),
        "the public OMP-only override must participate in live detection"
    );

    let ctx = ScanContext::with_roots(
        state_dir.join("cass-state"),
        vec![
            ScanRoot::local(omp_root.clone()),
            ScanRoot::local(shared_pi_root.clone()),
        ],
        None,
    );
    let omp_conversations = runtime_connector("omp")
        .scan(&ctx)
        .expect("scan the OMP-only override");
    let pi_conversations = runtime_connector("pi_agent")
        .scan(&ctx)
        .expect("scan the ambiguous shared Pi override");

    assert!(
        omp_conversations.iter().any(|conversation| {
            conversation.external_id.as_deref() == Some("omp-only-live-root")
        }),
        "OMP-only live-root conversation missing; scan returned: {:?}",
        omp_conversations
            .iter()
            .map(|conversation| (
                conversation.external_id.clone(),
                conversation.source_path.clone()
            ))
            .collect::<Vec<_>>()
    );
    assert!(
        !omp_conversations
            .iter()
            .any(|conversation| { conversation.external_id.as_deref() == Some("shared-pi-root") })
    );
    assert!(
        pi_conversations
            .iter()
            .any(|conversation| { conversation.external_id.as_deref() == Some("shared-pi-root") })
    );
    assert!(
        !pi_conversations.iter().any(|conversation| {
            conversation.external_id.as_deref() == Some("omp-only-live-root")
        })
    );
    eprintln!("{OMP_LIVE_OVERRIDE_CHILD_RECEIPT}");
}

#[test]
fn broad_root_partitions_pi_and_omp_sessions_once_each() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("copied-home");
    write_omp_session(
        &home.join(".pi/agent"),
        "pi-in-broad-root",
        "Broad-root Pi session",
    );
    write_omp_session(
        &home.join(".omp/agent"),
        "omp-in-broad-root",
        "Broad-root OMP session",
    );
    let ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::local(home)],
        None,
    );

    let pi_conversations = runtime_connector("pi_agent")
        .scan(&ctx)
        .expect("scan Pi from broad copied home");
    let omp_conversations = runtime_connector("omp")
        .scan(&ctx)
        .expect("scan OMP from broad copied home");

    assert_eq!(pi_conversations.len(), 1);
    assert_eq!(pi_conversations[0].agent_slug, "pi_agent");
    assert_eq!(omp_conversations.len(), 1);
    assert_eq!(omp_conversations[0].agent_slug, "omp");
    assert_ne!(
        pi_conversations[0].source_path,
        omp_conversations[0].source_path
    );
}

#[test]
fn omp_remote_discovery_preserves_origin_and_platform() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("remote-home");
    write_omp_session(&home.join(".omp/agent"), "omp-remote", "Remote OMP");
    let ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::remote(
            home,
            Origin::remote_with_host("build-host", "build-host.example"),
            Some(Platform::Linux),
        )],
        None,
    );

    let sources = OmpConnector::new()
        .discover_source_files(&ctx)
        .expect("discover remote OMP fixture");
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].provider_slug, "omp");
    assert!(sources[0].origin.is_remote());
    assert_eq!(sources[0].platform, Some(Platform::Linux));
}

#[test]
fn omp_token_extraction_uses_the_pi_family_model_schema() {
    let usage = extract_tokens_for_agent(
        "omp",
        &json!({"message":{"model":"openrouter/stealth/ox-alpha"}}),
        "answer",
        "assistant",
    );
    assert_eq!(
        usage.model_name.as_deref(),
        Some("openrouter/stealth/ox-alpha")
    );
    assert_eq!(usage.provider.as_deref(), Some("openrouter"));
}
