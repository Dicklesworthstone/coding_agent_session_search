//! `cass forget --apply` must stop every search surface from returning the
//! forgotten conversations (bead coding_agent_session_search-2l1b0.50).
//!
//! Before the fix, forget deleted the canonical rows and rebuilt FTS and
//! analytics, but left the Quill lexical generation untouched. Quill documents
//! store message content, so a plain search kept returning the forgotten text
//! until some later `cass index` rebuilt the index.
//!
//! Forget does not rewrite semantic vectors; it drops the semantic embed
//! watermark. Explicit semantic search then fails closed
//! (`semantic-unavailable`) until `cass index --semantic` re-embeds from the
//! canonical rows; the surface test proves no surface leaks meanwhile and
//! that the catch-up restores semantic search.
//!
//! Each step logs one JSON line on stderr: step, command, exit code, elapsed.

use assert_cmd::Command;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Instant;
use tempfile::TempDir;
use walkdir::WalkDir;

mod util;
use util::cass_bin;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const FORGOTTEN_MARKER: &str = "forgetmarkeralpha";
const KEPT_MARKER: &str = "keepmarkerbeta";
const SIBLING_MARKER: &str = "siblingmarkerdelta";
const LATE_MARKER: &str = "latemarkergamma";
const FORGOTTEN_APPEND_MARKER: &str = "forgottenappendmarkerepsilon";
/// The forgotten rollout's session id, which appears in every rendering of it.
const FORGOTTEN_SESSION: &str = "forget-a";

const LEXICAL: &[&str] = &["--mode", "lexical"];
/// Every search surface a hash-tier archive serves without a model download.
const SURFACES: &[(&str, &[&str])] = &[
    ("lexical", LEXICAL),
    ("semantic", &["--mode", "semantic", "--model", "hash"]),
    (
        "semantic-fast-only",
        &["--mode", "semantic", "--model", "hash", "--fast-only"],
    ),
    ("hybrid", &["--mode", "hybrid", "--model", "hash"]),
];

struct Archive {
    home: TempDir,
    data_dir: PathBuf,
}

impl Archive {
    fn cmd(&self) -> Command {
        let mut cmd = Command::new(cass_bin());
        cmd.env("HOME", self.home.path())
            .env("CODEX_HOME", self.home.path().join(".codex"))
            .env("CLAUDE_CONFIG_DIR", self.home.path().join(".claude"))
            .env("XDG_DATA_HOME", self.home.path().join(".local/share"))
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env(
                "OPENCODE_SQLITE_DB",
                self.home.path().join(".local/share/opencode/opencode.db"),
            )
            .env(
                "CASS_DEVIN_DATA_ROOT",
                self.home.path().join(".local/share/devin/cli"),
            )
            .env_remove("CASS_EXCLUDE_PATHS")
            .env_remove("CASS_EXCLUDE_WORKSPACES")
            .env("CASS_DATA_DIR", &self.data_dir)
            .env("CASS_AUTO_REFRESH", "0")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .current_dir(self.home.path());
        cmd
    }

    /// Run one step and log it; the caller judges the exit status.
    fn run_step(&self, step: &str, mut cmd: Command, args: &[&str]) -> TestResult<Output> {
        let started = Instant::now();
        let output = cmd.args(args).output()?;
        eprintln!(
            "{}",
            json!({
                "test": "cli_forget",
                "step": step,
                "command": args,
                "exit": output.status.code(),
                "elapsed_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "stderr_tail": String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or_default(),
            })
        );
        Ok(output)
    }

    fn succeed(&self, step: &str, args: &[&str]) -> TestResult<Output> {
        let output = self.run_step(step, self.cmd(), args)?;
        if !output.status.success() {
            return Err(
                format!("{step} failed: {}", String::from_utf8_lossy(&output.stderr)).into(),
            );
        }
        Ok(output)
    }

    /// `(source_path, content)` of every hit for `query` on one surface.
    fn search_hits(&self, query: &str, surface: &[&str]) -> TestResult<Vec<(String, String)>> {
        self.search_hits_or_unavailable(query, surface)?
            .ok_or_else(|| format!("search {} is semantic-unavailable", surface.join(" ")).into())
    }

    /// Like `search_hits`, but `None` when the surface fails closed with
    /// exit 15 `semantic-unavailable`.
    fn search_hits_or_unavailable(
        &self,
        query: &str,
        surface: &[&str],
    ) -> TestResult<Option<Vec<(String, String)>>> {
        let mut args = vec!["search", query, "--robot", "--limit", "10"];
        args.extend_from_slice(surface);
        let step = format!("search {}", surface.join(" "));
        let output = self.run_step(&step, self.cmd(), &args)?;
        if output.status.code() == Some(15) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let envelope: Value = serde_json::from_str(
                stderr
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .ok_or("exit 15 without an error envelope")?,
            )?;
            if envelope["error"]["kind"] == "semantic-unavailable" {
                return Ok(None);
            }
        }
        if !output.status.success() {
            return Err(
                format!("{step} failed: {}", String::from_utf8_lossy(&output.stderr)).into(),
            );
        }
        let payload: Value = serde_json::from_slice(&output.stdout)?;
        let hits = payload["hits"]
            .as_array()
            .ok_or("search payload has no hits array")?;
        Ok(Some(
            hits.iter()
                .map(|hit| {
                    (
                        hit["source_path"].as_str().unwrap_or_default().to_string(),
                        hit["content"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect(),
        ))
    }

    fn search_hit_paths(&self, marker: &str) -> TestResult<Vec<String>> {
        Ok(self
            .search_hits(marker, LEXICAL)?
            .into_iter()
            .map(|(path, _)| path)
            .collect())
    }

    fn pack(&self, query: &str) -> TestResult<String> {
        let output = self.succeed("pack", &["pack", query, "--json"])?;
        Ok(String::from_utf8(output.stdout)?)
    }

    /// The forget report; `--apply` only when `apply`.
    fn forget(&self, source_glob: &str, apply: bool) -> TestResult<Value> {
        self.forget_selected(&["--source-glob", source_glob], apply)
    }

    fn forget_selected(&self, selectors: &[&str], apply: bool) -> TestResult<Value> {
        let mut args = vec!["forget", "--json"];
        args.extend_from_slice(selectors);
        if apply {
            args.push("--apply");
        }
        let step = if apply {
            "forget --apply"
        } else {
            "forget dry-run"
        };
        let output = self.succeed(step, &args)?;
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    fn index(&self, extra: &[&str]) -> TestResult {
        let mut args = vec!["index", "--json", "--no-progress-events"];
        args.extend_from_slice(extra);
        self.succeed(&format!("index {}", extra.join(" ")), &args)?;
        Ok(())
    }

    /// Whether `cass status` reports the semantic assets current for the
    /// canonical database. A hash-only archive has no quality tier, so the
    /// check is no lexical fallback and a fast tier matching the database.
    fn semantic_status_is_current(&self) -> TestResult<bool> {
        let output = self.succeed("status", &["status", "--json"])?;
        let status: Value = serde_json::from_slice(&output.stdout)?;
        let semantic = &status["semantic"];
        eprintln!(
            "{}",
            json!({
                "test": "cli_forget",
                "step": "status semantic",
                "status": semantic["status"],
                "fallback_mode": semantic["fallback_mode"],
                "fast_tier_current_db_matches": semantic["fast_tier"]["current_db_matches"],
                "summary": semantic["summary"],
            })
        );
        Ok(semantic["fallback_mode"].is_null()
            && semantic["fast_tier"]["current_db_matches"] == true)
    }
}

/// A real-format Codex rollout (the connector only reads `rollout-*.jsonl`).
fn write_codex_rollout(codex_home: &Path, name: &str, marker: &str) -> TestResult<PathBuf> {
    let dir = codex_home.join("sessions/2026/09/20");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("rollout-2026-09-20T10-00-00-{name}.jsonl"));
    let lines = [
        format!(
            r#"{{"timestamp":"2026-09-20T10:00:00.000Z","type":"session_meta","payload":{{"id":"{name}","cwd":"/work/forget-test","cli_version":"0.42.0"}}}}"#
        ),
        format!(
            r#"{{"timestamp":"2026-09-20T10:00:01.000Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"please remember {marker} for later"}}]}}}}"#
        ),
        format!(
            r#"{{"timestamp":"2026-09-20T10:00:02.000Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"text","text":"noted {marker} in the notes"}}]}}}}"#
        ),
    ];
    fs::write(&path, lines.join("\n") + "\n")?;
    Ok(path)
}

/// Two indexed rollouts; `index_args` extends `cass index --full`.
fn indexed_archive(index_args: &[&str]) -> TestResult<(Archive, PathBuf)> {
    let home = TempDir::new()?;
    let data_dir = home.path().join("cass-data");
    fs::create_dir_all(&data_dir)?;
    let codex_home = home.path().join(".codex");
    let forgotten = write_codex_rollout(&codex_home, FORGOTTEN_SESSION, FORGOTTEN_MARKER)?;
    write_codex_rollout(&codex_home, "keep-b", KEPT_MARKER)?;
    let archive = Archive { home, data_dir };
    let mut full = vec!["--full"];
    full.extend_from_slice(index_args);
    archive.index(&full)?;
    Ok((archive, forgotten))
}

fn write_workspace_rollout(
    archive: &Archive,
    name: &str,
    marker: &str,
    workspace: &str,
    timestamp: chrono::DateTime<chrono::Utc>,
) -> TestResult<PathBuf> {
    let dir = archive.home.path().join(".codex/sessions/2026/01/01");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("rollout-{name}.jsonl"));
    let events = [
        json!({
            "timestamp": timestamp.to_rfc3339(), "type": "session_meta",
            "payload": { "id": name, "cwd": workspace, "cli_version": "0.42.0" }
        }),
        json!({
            "timestamp": timestamp.to_rfc3339(), "type": "response_item",
            "payload": { "type": "message", "role": "user", "content": [
                { "type": "input_text", "text": format!("remember {marker}") }
            ] }
        }),
    ];
    let mut file = fs::File::create(&path)?;
    for event in events {
        writeln!(file, "{event}")?;
    }
    Ok(path)
}

fn workspace_archive() -> TestResult<Archive> {
    let home = TempDir::new()?;
    let data_dir = home.path().join("cass-data");
    fs::create_dir_all(&data_dir)?;
    let archive = Archive { home, data_dir };
    let now = chrono::Utc::now();
    write_workspace_rollout(
        &archive,
        "old-scratch",
        FORGOTTEN_MARKER,
        "/work/scratch/retired",
        now - chrono::Duration::days(180),
    )?;
    write_workspace_rollout(
        &archive,
        "old-kept",
        KEPT_MARKER,
        "/work/production",
        now - chrono::Duration::days(180),
    )?;
    write_workspace_rollout(
        &archive,
        "recent-scratch",
        LATE_MARKER,
        "/work/scratch/current",
        now,
    )?;
    archive.index(&["--full"])?;
    for marker in [FORGOTTEN_MARKER, KEPT_MARKER, LATE_MARKER] {
        assert!(
            !archive.search_hit_paths(marker)?.is_empty(),
            "fixture missing: {marker}"
        );
    }
    Ok(archive)
}

#[test]
fn forget_workspace_preview_then_apply_selects_recorded_workspace() -> TestResult {
    let archive = workspace_archive()?;
    let selectors = ["--workspace-glob", "/work/scratch/*"];
    let before = derived_assets(&archive.data_dir)?;
    let preview = archive.forget_selected(&selectors, false)?;
    assert_eq!(preview["conversations_matched"], 2);
    assert_eq!(preview["conversations_deleted"], 0);
    assert_eq!(preview["workspace_glob"], "/work/scratch/*");
    assert_eq!(before, derived_assets(&archive.data_dir)?);
    assert!(!archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    let applied = archive.forget_selected(&selectors, true)?;
    assert_eq!(applied["conversations_deleted"], 2);
    assert!(archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    assert!(archive.search_hit_paths(LATE_MARKER)?.is_empty());
    assert!(!archive.search_hit_paths(KEPT_MARKER)?.is_empty());
    Ok(())
}

#[test]
fn forget_age_preview_then_apply_keeps_recent_messages() -> TestResult {
    let archive = workspace_archive()?;
    let selectors = ["--older-than", "90d"];
    let preview = archive.forget_selected(&selectors, false)?;
    assert_eq!(preview["conversations_matched"], 2);
    assert_eq!(preview["conversations_deleted"], 0);
    assert_eq!(preview["older_than"], "90d");
    assert!(preview["before_ms"].as_i64().is_some());
    assert!(!archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    let applied = archive.forget_selected(&selectors, true)?;
    assert_eq!(applied["conversations_deleted"], 2);
    assert!(archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    assert!(archive.search_hit_paths(KEPT_MARKER)?.is_empty());
    assert!(!archive.search_hit_paths(LATE_MARKER)?.is_empty());
    Ok(())
}

#[test]
fn forget_combines_source_workspace_and_age_selectors() -> TestResult {
    let archive = workspace_archive()?;
    let selectors = [
        "--source-glob",
        "**/rollout-old-*.jsonl",
        "--workspace-glob",
        "/work/scratch/*",
        "--older-than",
        "90d",
    ];
    let preview = archive.forget_selected(&selectors, false)?;
    assert_eq!(preview["conversations_matched"], 1);
    assert_eq!(
        archive.forget_selected(&selectors, true)?["conversations_deleted"],
        1
    );
    assert!(archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    assert!(!archive.search_hit_paths(KEPT_MARKER)?.is_empty());
    assert!(!archive.search_hit_paths(LATE_MARKER)?.is_empty());
    archive.index(&["--full"])?;
    assert!(archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    assert!(!archive.search_hit_paths(KEPT_MARKER)?.is_empty());
    Ok(())
}

fn insert_opencode_message(
    conn: &coding_agent_search::franken_sync::Connection,
    session: &str,
    message: &str,
    marker: &str,
    timestamp: i64,
) -> TestResult {
    use coding_agent_search::franken_sync::{compat::ConnectionExt, params};
    conn.execute_compat(
        "INSERT INTO message (id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?3, ?4)",
        params![
            message,
            session,
            timestamp,
            json!({ "role": "user" }).to_string()
        ],
    )?;
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
        params![
            format!("part-{message}"),
            message,
            session,
            timestamp,
            json!({ "type": "text", "text": format!("remember {marker}") }).to_string()
        ],
    )?;
    conn.execute_compat(
        "UPDATE session SET time_updated = ?1 WHERE id = ?2",
        params![timestamp, session],
    )?;
    Ok(())
}

fn assert_shared_session_tombstone(archive: &Archive, source: &Path, provider: &str) -> TestResult {
    use coding_agent_search::storage::sqlite::{ForgottenConversationKey, FrankenStorage};

    let storage = FrankenStorage::open_readonly(&archive.data_dir.join("agent_search.db"))?;
    let keys = storage.forgotten_conversation_keys()?;
    assert_eq!(keys.len(), 1);
    assert!(keys.contains(&ForgottenConversationKey {
        source_id: "local".into(),
        agent_slug: provider.into(),
        source_path: source.join("ses-forgotten").to_string_lossy().into_owned(),
        external_id: "ses-forgotten".into(),
    }));
    assert!(
        storage.forgotten_source_stamps()?.is_empty(),
        "logical database/session paths need real session identities, not absent-file stamps"
    );
    storage.close_without_checkpoint()?;
    Ok(())
}

fn index_shared_with_raw_capture(archive: &Archive, extra: &[&str]) -> TestResult {
    let mut cmd = archive.cmd();
    cmd.env("CASS_RAW_MIRROR", "1")
        .env("CASS_RAW_MIRROR_AUTO_PRUNE", "0")
        .env("CASS_RAW_MIRROR_MIN_FREE_BYTES", "0")
        .env("CASS_RAW_MIRROR_MAX_BYTES", "67108864");
    let mut args = vec!["index", "--json", "--no-progress-events"];
    args.extend_from_slice(extra);
    let output = archive.run_step("index shared store with raw capture enabled", cmd, &args)?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// Capture identities for a physical database and its SQLite sidecars.
/// Existing manifests may gain links; only a new capture violates exclusion.
fn raw_source_manifest_ids(data_dir: &Path, source: &Path) -> TestResult<BTreeMap<String, String>> {
    let root = data_dir.join("raw-mirror/v1/manifests");
    let mut captures = BTreeMap::new();
    if !root.exists() {
        return Ok(captures);
    }
    let source = source.to_string_lossy();
    for entry in WalkDir::new(root) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let manifest: Value = serde_json::from_slice(&fs::read(entry.path())?)?;
        let original_path = manifest["original_path"]
            .as_str()
            .ok_or("raw manifest has no original_path")?;
        if original_path == source.as_ref()
            || original_path
                .strip_prefix(source.as_ref())
                .is_some_and(|suffix| matches!(suffix, "-wal" | "-shm" | "-journal"))
        {
            let id = manifest["manifest_id"]
                .as_str()
                .ok_or("raw manifest has no manifest_id")?;
            captures.insert(id.to_string(), original_path.to_string());
        }
    }
    Ok(captures)
}

fn apply_shared_session_forget(
    archive: &Archive,
    source: &Path,
    provider: &str,
) -> TestResult<Vec<u8>> {
    assert!(!archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    assert!(!archive.search_hit_paths(KEPT_MARKER)?.is_empty());
    assert!(
        raw_source_manifest_ids(&archive.data_dir, source)?
            .values()
            .any(|path| path.as_str() == source.to_string_lossy().as_ref()),
        "negative control: the physical shared database must have been raw-captured"
    );
    assert!(raw_mirror_holds(&archive.data_dir, FORGOTTEN_MARKER)?);
    assert!(raw_mirror_holds(&archive.data_dir, KEPT_MARKER)?);
    let source_before = fs::read(source)?;
    let assets_before = derived_assets(&archive.data_dir)?;
    let selectors = ["--workspace-glob", "/work/scratch/*"];
    let preview = archive.forget_selected(&selectors, false)?;
    assert_eq!(preview["conversations_matched"], 1);
    assert_eq!(preview["messages_matched"], 1);
    assert_eq!(preview["conversations_deleted"], 0);
    assert_eq!(preview["dry_run"], true);
    assert_eq!(fs::read(source)?, source_before);
    assert_eq!(derived_assets(&archive.data_dir)?, assets_before);
    assert_eq!(
        archive.forget_selected(&selectors, true)?["conversations_deleted"],
        1
    );
    assert_eq!(
        fs::read(source)?,
        source_before,
        "forget rewrote the source store"
    );
    assert!(archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    assert!(!archive.search_hit_paths(KEPT_MARKER)?.is_empty());
    assert_shared_session_tombstone(archive, source, provider)?;
    Ok(source_before)
}

fn assert_shared_forget_survives_rescans(
    archive: &Archive,
    source: &Path,
    original_captures: &BTreeMap<String, String>,
) -> TestResult {
    let source_before = fs::read(source)?;
    for extra in [&[][..], &["--full"][..]] {
        index_shared_with_raw_capture(archive, extra)?;
        assert!(
            archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty(),
            "index {extra:?} resurrected a forgotten shared-store session"
        );
        assert!(
            archive
                .search_hit_paths(FORGOTTEN_APPEND_MARKER)?
                .is_empty(),
            "own-message changes must not clear a durable session identity"
        );
        assert!(!archive.search_hit_paths(KEPT_MARKER)?.is_empty());
        assert!(
            !archive.search_hit_paths(LATE_MARKER)?.is_empty(),
            "index {extra:?} suppressed the retained sibling's new message"
        );
        assert_eq!(
            raw_source_manifest_ids(&archive.data_dir, source)?,
            *original_captures,
            "index {extra:?} recaptured a database containing a forgotten session"
        );
        for marker in [LATE_MARKER, FORGOTTEN_APPEND_MARKER] {
            assert!(
                !raw_mirror_holds(&archive.data_dir, marker)?,
                "index {extra:?} raw-captured changed shared-store bytes"
            );
        }
        assert_eq!(fs::read(source)?, source_before);
    }
    Ok(())
}

fn indexed_opencode_archive(home: TempDir) -> TestResult<(Archive, PathBuf)> {
    indexed_opencode_archive_with_sessions(
        home,
        &[
            ("ses-forgotten", "/work/scratch/retired", FORGOTTEN_MARKER),
            ("ses-kept", "/work/production", KEPT_MARKER),
        ],
    )
}

fn indexed_opencode_archive_with_sessions(
    home: TempDir,
    sessions: &[(&str, &str, &str)],
) -> TestResult<(Archive, PathBuf)> {
    use coding_agent_search::franken_sync::{Connection, compat::ConnectionExt, params};

    let data_dir = home.path().join("cass-data");
    let source = home.path().join(".local/share/opencode/opencode.db");
    fs::create_dir_all(source.parent().ok_or("source has no parent")?)?;
    let conn = Connection::open(source.to_string_lossy().as_ref())?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE session (
            id TEXT PRIMARY KEY, project_id TEXT NOT NULL, slug TEXT NOT NULL,
            directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL
         );
         CREATE TABLE message (
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL
         );
         CREATE TABLE part (
            id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL
         );",
    )?;
    let timestamp = chrono::Utc::now().timestamp_millis() - 86_400_000;
    for &(session, workspace, marker) in sessions {
        conn.execute_compat(
            "INSERT INTO session
             (id, project_id, slug, directory, title, version, time_created, time_updated)
             VALUES (?1, ?1, ?1, ?2, ?1, '1.14.46', ?3, ?3)",
            params![session, workspace, timestamp],
        )?;
        insert_opencode_message(&conn, session, &format!("msg-{session}"), marker, timestamp)?;
    }
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(conn);

    let archive = Archive { home, data_dir };
    index_shared_with_raw_capture(&archive, &["--full"])?;
    Ok((archive, source))
}

/// The real shared-store negative control from GH #537: a retained sibling
/// changes the database's file stamp, but must not resurrect the forgotten ID.
#[test]
fn forgotten_opencode_session_stays_out_when_sibling_changes() -> TestResult {
    use coding_agent_search::franken_sync::Connection;

    let (archive, source) = indexed_opencode_archive(TempDir::new()?)?;
    let source_before = apply_shared_session_forget(&archive, &source, "opencode")?;
    let original_captures = raw_source_manifest_ids(&archive.data_dir, &source)?;

    let conn = Connection::open(source.to_string_lossy().as_ref())?;
    insert_opencode_message(
        &conn,
        "ses-kept",
        "msg-kept-appended",
        LATE_MARKER,
        chrono::Utc::now().timestamp_millis() + 2_000,
    )?;
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(conn);

    assert_ne!(
        fs::read(&source)?,
        source_before,
        "sibling must actually change the store"
    );
    assert_shared_forget_survives_rescans(&archive, &source, &original_captures)?;

    let conn = Connection::open(source.to_string_lossy().as_ref())?;
    insert_opencode_message(
        &conn,
        "ses-forgotten",
        "msg-forgotten-appended",
        FORGOTTEN_APPEND_MARKER,
        chrono::Utc::now().timestamp_millis() + 2_000,
    )?;
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(conn);
    assert_shared_forget_survives_rescans(&archive, &source, &original_captures)?;
    assert_shared_session_tombstone(&archive, &source, "opencode")?;
    Ok(())
}

#[test]
fn shared_session_reinclusion_requires_apply_and_preserves_other_exclusions() -> TestResult {
    let home = tempfile::Builder::new()
        .prefix("cass-reinclude-[literal]-")
        .tempdir()?;
    let (archive, source) = indexed_opencode_archive(home)?;
    let source_before = apply_shared_session_forget(&archive, &source, "opencode")?;
    assert_eq!(
        archive.forget_selected(&["--workspace-glob", "/work/production"], true)?["conversations_deleted"],
        1
    );
    let all = archive.forget_selected(&["--list"], false)?;
    assert_eq!(all["schema_version"], 1);
    assert_eq!(all["mode"], "list");
    assert_eq!(all["applied"], false);
    assert_eq!(all["exclusions_count"], 2);
    assert_eq!(all["legacy_sources_count"], 0);
    assert_eq!(
        all["exclusions"]
            .as_array()
            .ok_or("no exclusions array")?
            .len(),
        2
    );
    assert_eq!(all["legacy_sources"], json!([]));

    let logical_source = source.join("ses-forgotten");
    let logical_source = logical_source.to_str().ok_or("non-utf8 logical source")?;
    let selectors = [
        "--source-path",
        logical_source,
        "--source-id",
        "local",
        "--agent",
        "opencode",
        "--external-id",
        "ses-forgotten",
    ];
    let mut list = vec!["--list"];
    list.extend_from_slice(&selectors);
    let assets_before = derived_assets(&archive.data_dir)?;
    let selected = archive.forget_selected(&list, false)?;
    let sessions = selected["exclusions"]
        .as_array()
        .ok_or("no exclusions array")?;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["source_path"], logical_source);
    assert_eq!(sessions[0]["source_id"], "local");
    assert_eq!(sessions[0]["agent_slug"], "opencode");
    assert_eq!(sessions[0]["external_id"], "ses-forgotten");
    assert_eq!(sessions[0]["blocked_by_source_stamp"], false);
    assert!(sessions[0]["forgotten_at_ms"].is_i64());
    assert_eq!(
        selected["selector"],
        json!({
            "source_glob": null,
            "source_path": logical_source,
            "source_id": "local",
            "agent_slug": "opencode",
            "external_id": "ses-forgotten"
        })
    );
    assert_eq!(
        archive.forget_selected(&["--list", "--source-id", "remote-other"], false)?["exclusions"],
        json!([])
    );

    let mut reinclude = vec!["--reinclude"];
    reinclude.extend_from_slice(&selectors);
    let preview = archive.forget_selected(&reinclude, false)?;
    assert_eq!(preview["schema_version"], 1);
    assert_eq!(preview["mode"], "reinclude");
    assert_eq!(preview["applied"], false);
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["exclusions_matched"], 1);
    assert_eq!(preview["exclusions_removed"], 0);
    assert_eq!(preview["legacy_sources_removed"], 0);
    assert_eq!(preview["recommended_next_command"], "cass index");
    assert_eq!(derived_assets(&archive.data_dir)?, assets_before);
    assert_eq!(fs::read(&source)?, source_before);

    index_shared_with_raw_capture(&archive, &["--full"])?;
    assert!(archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    assert!(archive.search_hit_paths(KEPT_MARKER)?.is_empty());
    let original_captures = raw_source_manifest_ids(&archive.data_dir, &source)?;
    let applied = archive.forget_selected(&reinclude, true)?;
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["dry_run"], false);
    assert_eq!(applied["exclusions_matched"], 1);
    assert_eq!(applied["exclusions_removed"], 1);
    assert_eq!(applied["legacy_sources_removed"], 0);
    let remaining = archive.forget_selected(&["--list"], false)?;
    let remaining = remaining["exclusions"]
        .as_array()
        .ok_or("no exclusions array")?;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["external_id"], "ses-kept");

    index_shared_with_raw_capture(&archive, &["--full"])?;
    assert!(!archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    assert!(archive.search_hit_paths(KEPT_MARKER)?.is_empty());
    assert_eq!(
        raw_source_manifest_ids(&archive.data_dir, &source)?,
        original_captures,
        "the remaining sibling exclusion must keep the shared raw container blocked"
    );
    assert_eq!(fs::read(&source)?, source_before);
    Ok(())
}

#[test]
fn reinclusion_recovers_unchanged_shared_session_with_incremental_index() -> TestResult {
    use coding_agent_search::storage::sqlite::FrankenStorage;

    let (archive, source) = indexed_opencode_archive_with_sessions(
        TempDir::new()?,
        &[
            ("ses-forgotten", "/work/scratch/retired", FORGOTTEN_MARKER),
            ("ses-still-forgotten", "/work/scratch/other", SIBLING_MARKER),
            ("ses-kept", "/work/production", KEPT_MARKER),
        ],
    )?;
    let source_before = fs::read(&source)?;
    assert_eq!(
        archive.forget_selected(&["--workspace-glob", "/work/scratch/*"], true)?["conversations_deleted"],
        2
    );
    // Retain one canonical conversation, so an empty-archive fallback cannot
    // hide a stale connector cutoff. Record a successful suppressed-source pass.
    archive.index(&[])?;
    assert!(archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    assert!(archive.search_hit_paths(SIBLING_MARKER)?.is_empty());
    assert!(!archive.search_hit_paths(KEPT_MARKER)?.is_empty());
    let logical_source = source.join("ses-forgotten");
    let selectors = [
        "--reinclude",
        "--source-path",
        logical_source.to_str().ok_or("non-UTF-8 fixture path")?,
        "--source-id",
        "local",
        "--agent",
        "opencode",
        "--external-id",
        "ses-forgotten",
    ];
    let db_path = archive.data_dir.join("agent_search.db");
    let database_before = fs::read(&db_path)?;
    let assets_before = derived_assets(&archive.data_dir)?;
    let preview = archive.forget_selected(&selectors, false)?;
    assert_eq!(preview["exclusions_matched"], 1);
    assert_eq!(preview["exclusions_removed"], 0);
    assert_eq!(preview["reindex_required"], false);
    assert_eq!(preview["conversations_restored"], 0);
    assert_eq!(fs::read(&db_path)?, database_before);
    assert_eq!(derived_assets(&archive.data_dir)?, assets_before);

    let applied = archive.forget_selected(&selectors, true)?;
    assert_eq!(applied["exclusions_removed"], 1);
    assert_eq!(applied["legacy_sources_removed"], 0);
    assert_eq!(applied["reindex_required"], true);
    assert_eq!(applied["conversations_restored"], 0);
    assert_eq!(
        applied["next_command_argv"],
        json!([
            "cass",
            "index",
            "--db",
            db_path,
            "--data-dir",
            archive.data_dir,
            "--json"
        ])
    );
    assert!(archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    archive.index(&[])?;
    assert!(!archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    assert!(archive.search_hit_paths(SIBLING_MARKER)?.is_empty());
    assert!(!archive.search_hit_paths(KEPT_MARKER)?.is_empty());
    assert_eq!(fs::read(&source)?, source_before);
    let storage = FrankenStorage::open_readonly(&db_path)?;
    assert_eq!(storage.total_conversation_count()?, 2);
    assert_eq!(storage.forgotten_conversation_keys()?.len(), 1);
    storage.close_without_checkpoint()?;
    let no_op = archive.forget_selected(&selectors, true)?;
    assert_eq!(no_op["exclusions_removed"], 0);
    assert_eq!(no_op["source_observations_removed"], 0);
    assert_eq!(no_op["reindex_required"], false);
    assert_eq!(no_op["next_command_argv"], Value::Null);
    archive.index(&["--full"])?;
    assert!(archive.search_hit_paths(SIBLING_MARKER)?.is_empty());
    Ok(())
}

#[test]
fn reinclusion_recovers_unchanged_file_incrementally_and_honors_exclusions() -> TestResult {
    let (archive, source) = indexed_archive(&[])?;
    let source_path = source.to_str().ok_or("non-UTF-8 fixture path")?;
    let source_before = fs::read(&source)?;
    assert_eq!(
        archive.forget(source_path, true)?["conversations_deleted"],
        1
    );
    archive.index(&[])?;
    let selectors = ["--reinclude", "--source-path", source_path];
    let preview = archive.forget_selected(&selectors, false)?;
    assert_eq!(preview["legacy_sources_matched"], 1);
    assert_eq!(preview["legacy_sources_removed"], 0);
    let applied = archive.forget_selected(&selectors, true)?;
    assert_eq!(applied["legacy_sources_removed"], 1);
    assert!(archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());

    let mut excluded = archive.cmd();
    excluded.env("CASS_EXCLUDE_PATHS", &source);
    let output = archive.run_step(
        "reinclude respects active source exclusion",
        excluded,
        &["index", "--json", "--no-progress-events"],
    )?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    archive.index(&[])?;
    assert!(!archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    assert!(!archive.search_hit_paths(KEPT_MARKER)?.is_empty());
    assert_eq!(fs::read(&source)?, source_before);
    Ok(())
}

#[test]
fn identity_reinclusion_refuses_an_overlapping_legacy_source_stamp() -> TestResult {
    use coding_agent_search::franken_sync::{compat::ConnectionExt, params};
    use coding_agent_search::storage::sqlite::FrankenStorage;

    let (archive, source) = indexed_opencode_archive(TempDir::new()?)?;
    let source_before = apply_shared_session_forget(&archive, &source, "opencode")?;
    let logical_source = source.join("ses-forgotten");
    let logical_source = logical_source.to_str().ok_or("non-utf8 logical source")?;
    let storage = FrankenStorage::open(&archive.data_dir.join("agent_search.db"))?;
    storage.raw().execute_compat(
        "INSERT INTO forgotten_sources(source_path, size_bytes, mtime_ms, forgotten_at_ms)
         VALUES (?1, NULL, NULL, 1)",
        params![logical_source],
    )?;
    storage.close()?;

    let selectors = [
        "--reinclude",
        "--source-path",
        logical_source,
        "--external-id",
        "ses-forgotten",
    ];
    let preview = archive.forget_selected(&selectors, false)?;
    assert_eq!(preview["exclusions_matched"], 1);
    assert_eq!(preview["exclusions_removed"], 0);
    assert_eq!(preview["legacy_sources_matched"], 0);
    assert_eq!(preview["blocked_by_source_stamps"], json!([logical_source]));
    let before = archive.forget_selected(&["--list"], false)?;
    assert_eq!(before["exclusions"][0]["blocked_by_source_stamp"], true);
    let assets_before = derived_assets(&archive.data_dir)?;
    let mut args = vec!["forget", "--json", "--apply"];
    args.extend_from_slice(&selectors);
    let refused = archive.run_step(
        "identity release blocked by source stamp",
        archive.cmd(),
        &args,
    )?;
    assert_eq!(refused.status.code(), Some(5));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    let envelope: Value = serde_json::from_str(
        stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .ok_or("no exclusion refusal envelope on stderr")?,
    )?;
    assert_eq!(envelope["error"]["kind"], "forget-exclusions");
    let after = archive.forget_selected(&["--list"], false)?;
    assert_eq!(after["exclusions"], before["exclusions"]);
    assert_eq!(after["legacy_sources"], before["legacy_sources"]);
    assert_eq!(derived_assets(&archive.data_dir)?, assets_before);

    // A source-only request explicitly selects both exclusion mechanisms.
    let released =
        archive.forget_selected(&["--reinclude", "--source-path", logical_source], true)?;
    assert_eq!(released["exclusions_removed"], 1);
    assert_eq!(released["legacy_sources_removed"], 1);
    index_shared_with_raw_capture(&archive, &["--full"])?;
    assert!(!archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    assert!(!archive.search_hit_paths(KEPT_MARKER)?.is_empty());
    assert_eq!(fs::read(&source)?, source_before);
    Ok(())
}

#[test]
fn exclusion_management_rejects_ambiguous_or_unsupported_selectors() -> TestResult {
    let home = TempDir::new()?;
    let data_dir = home.path().join("cass-data");
    let archive = Archive { home, data_dir };
    for args in [
        vec!["forget", "--list", "--apply", "--json"],
        vec!["forget", "--list", "--workspace-glob", "/work/*", "--json"],
        vec!["forget", "--list", "--older-than", "30d", "--json"],
        vec!["forget", "--reinclude", "--json"],
        vec![
            "forget",
            "--reinclude",
            "--workspace-glob",
            "/work/*",
            "--json",
        ],
        vec!["forget", "--reinclude", "--older-than", "30d", "--json"],
        vec!["forget", "--source-path", "/transcript.jsonl", "--json"],
        vec!["forget", "--source-id", "local", "--json"],
        vec!["forget", "--agent", "opencode", "--json"],
        vec!["forget", "--external-id", "session", "--json"],
        vec![
            "forget",
            "--list",
            "--source-path",
            "/transcript.jsonl",
            "--source-glob",
            "*",
            "--json",
        ],
    ] {
        let output = archive.run_step("reject invalid exclusion mode", archive.cmd(), &args)?;
        assert_eq!(
            output.status.code(),
            Some(2),
            "arguments must be rejected: {args:?}"
        );
    }
    Ok(())
}

fn insert_devin_message(
    conn: &coding_agent_search::franken_sync::Connection,
    session: &str,
    node: i64,
    parent: Option<i64>,
    marker: &str,
    timestamp_ms: i64,
) -> TestResult {
    use coding_agent_search::franken_sync::{compat::ConnectionExt, params};

    let timestamp = timestamp_ms / 1_000;
    conn.execute_compat(
        "INSERT INTO message_nodes (node_id, session_id, parent_node_id, chat_message, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            node,
            session,
            parent,
            json!({
                "message_id": format!("message-{node}"),
                "role": "user",
                "content": format!("remember {marker}"),
                "metadata": { "is_user_input": true }
            })
            .to_string(),
            timestamp
        ],
    )?;
    conn.execute_compat(
        "UPDATE sessions SET last_activity_at = ?1, main_chain_id = ?2 WHERE id = ?3",
        params![timestamp, node, session],
    )?;
    Ok(())
}

#[test]
fn forgotten_devin_session_stays_out_when_sibling_or_own_messages_change() -> TestResult {
    use coding_agent_search::franken_sync::{Connection, compat::ConnectionExt, params};

    let home = TempDir::new()?;
    let data_dir = home.path().join("cass-data");
    let source = home.path().join(".local/share/devin/cli/sessions.db");
    fs::create_dir_all(source.parent().ok_or("source has no parent")?)?;
    let conn = Connection::open(source.to_string_lossy().as_ref())?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE sessions (
            id TEXT PRIMARY KEY, title TEXT, working_directory TEXT,
            model TEXT, agent_mode TEXT, created_at INTEGER, last_activity_at INTEGER,
            main_chain_id INTEGER, hidden INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE message_nodes (
            node_id INTEGER PRIMARY KEY, session_id TEXT NOT NULL, parent_node_id INTEGER,
            chat_message TEXT, created_at INTEGER
         );",
    )?;
    let timestamp_ms = chrono::Utc::now().timestamp_millis() - 86_400_000;
    for (session, workspace, node, marker) in [
        (
            "ses-forgotten",
            "/work/scratch/retired",
            1,
            FORGOTTEN_MARKER,
        ),
        ("ses-kept", "/work/production", 2, KEPT_MARKER),
    ] {
        conn.execute_compat(
            "INSERT INTO sessions
                 (id, title, working_directory, model, agent_mode, created_at, last_activity_at)
             VALUES (?1, ?1, ?2, 'swe-1', 'default', ?3, ?3)",
            params![session, workspace, timestamp_ms / 1_000],
        )?;
        insert_devin_message(&conn, session, node, None, marker, timestamp_ms)?;
    }
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(conn);

    let archive = Archive { home, data_dir };
    index_shared_with_raw_capture(&archive, &["--full"])?;
    let source_before = apply_shared_session_forget(&archive, &source, "devin")?;
    let original_captures = raw_source_manifest_ids(&archive.data_dir, &source)?;
    let conn = Connection::open(source.to_string_lossy().as_ref())?;
    insert_devin_message(
        &conn,
        "ses-kept",
        3,
        Some(2),
        LATE_MARKER,
        // Devin stores seconds. Clear the previous millisecond watermark
        // even when the commands run inside the same wall-clock second.
        chrono::Utc::now().timestamp_millis() + 2_000,
    )?;
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(conn);
    assert_ne!(
        fs::read(&source)?,
        source_before,
        "sibling must actually change the store"
    );
    assert_shared_forget_survives_rescans(&archive, &source, &original_captures)?;

    let conn = Connection::open(source.to_string_lossy().as_ref())?;
    insert_devin_message(
        &conn,
        "ses-forgotten",
        4,
        Some(1),
        FORGOTTEN_APPEND_MARKER,
        chrono::Utc::now().timestamp_millis() + 2_000,
    )?;
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(conn);
    assert_shared_forget_survives_rescans(&archive, &source, &original_captures)?;
    assert_shared_session_tombstone(&archive, &source, "devin")?;
    Ok(())
}

#[test]
fn workspace_exclusion_advances_scan_and_removal_revisits_unchanged_source() -> TestResult {
    use coding_agent_search::storage::sqlite::FrankenStorage;

    let home = TempDir::new()?;
    let data_dir = home.path().join("cass-data");
    let archive = Archive { home, data_dir };
    let source = write_workspace_rollout(
        &archive,
        "excluded-workspace",
        FORGOTTEN_MARKER,
        "/work/scratch/retired",
        chrono::Utc::now() - chrono::Duration::days(180),
    )?;
    let source_before = fs::read(&source)?;
    let mut previous_scan = None;
    for (iteration, extra) in [&["--full"][..], &[][..]].into_iter().enumerate() {
        let mut cmd = archive.cmd();
        cmd.env("CASS_EXCLUDE_WORKSPACES", "/work/scratch/*");
        let mut args = vec!["index", "--json", "--no-progress-events"];
        args.extend_from_slice(extra);
        let output = archive.run_step("index with workspace exclusion", cmd, &args)?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout)?;
        if iteration > 0 {
            assert_eq!(report["indexing_stats"]["total_conversations"], 0);
        }
        let storage = FrankenStorage::open_readonly(&archive.data_dir.join("agent_search.db"))?;
        assert_eq!(storage.total_conversation_count()?, 0);
        let states = storage.connector_scan_states(&["codex"])?;
        let scan = states["codex"]
            .last_scan_ts
            .ok_or("excluded source withheld scan watermark")?;
        if let Some(previous) = previous_scan {
            assert!(scan >= previous, "scan watermark moved backwards");
        }
        previous_scan = Some(scan);
        assert!(!storage.source_ingest_ledger_entries()?.is_empty());
        storage.close_without_checkpoint()?;
    }

    // No source mutation: clearing the policy itself must invalidate the
    // saved completion and make the previously excluded session searchable.
    assert_eq!(fs::read(&source)?, source_before);
    archive.index(&[])?;
    assert!(!archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    Ok(())
}

/// Every file under the data dir except the canonical database family, by
/// relative path: the derived assets a dry run must leave byte-identical.
fn derived_assets(data_dir: &Path) -> TestResult<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for entry in WalkDir::new(data_dir) {
        let entry = entry?;
        if !entry.file_type().is_file() || entry.file_name().to_string_lossy().contains(".db") {
            continue;
        }
        files.insert(
            entry.path().strip_prefix(data_dir)?.to_path_buf(),
            fs::read(entry.path())?,
        );
    }
    Ok(files)
}

#[test]
fn forget_apply_removes_forgotten_text_from_lexical_search() -> TestResult {
    let (archive, forgotten) = indexed_archive(&[])?;
    let forgotten_str = forgotten.to_str().ok_or("non-utf8 fixture path")?;

    // Negative control: the marker is searchable before forget, so an empty
    // result afterwards is caused by forget and not by a broken fixture.
    let before = archive.search_hit_paths(FORGOTTEN_MARKER)?;
    if !before.iter().any(|path| path == forgotten_str) {
        return Err(format!("fixture not indexed; hits before forget: {before:?}").into());
    }

    // A dry run changes nothing: the canonical rows still match and every
    // derived asset is byte-identical.
    let assets_before = derived_assets(&archive.data_dir)?;
    let dry_run = archive.forget(forgotten_str, false)?;
    if dry_run["conversations_matched"] != 1 || dry_run["conversations_deleted"] != 0 {
        return Err(format!("unexpected dry-run report: {dry_run}").into());
    }
    let assets_after = derived_assets(&archive.data_dir)?;
    if assets_before != assets_after {
        let changed: Vec<_> = assets_before
            .keys()
            .chain(assets_after.keys())
            .filter(|path| assets_before.get(*path) != assets_after.get(*path))
            .collect();
        return Err(format!("a forget dry run changed derived assets: {changed:?}").into());
    }
    if archive.forget(forgotten_str, false)?["conversations_matched"] != 1 {
        return Err("a forget dry run deleted canonical rows".into());
    }
    if archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty() {
        return Err("a forget dry run must not change search results".into());
    }

    let report = archive.forget(forgotten_str, true)?;
    if report["conversations_deleted"] != 1 {
        return Err(format!("expected one deleted conversation: {report}").into());
    }

    let after = archive.search_hit_paths(FORGOTTEN_MARKER)?;
    if !after.is_empty() {
        return Err(format!("forgotten text is still searchable: {after:?}").into());
    }
    let kept = archive.search_hit_paths(KEPT_MARKER)?;
    if kept.is_empty() {
        return Err("forget removed an unrelated conversation from search".into());
    }

    // Forget removes indexed copies. The raw mirror keeps its verbatim
    // capture of the source; the README forget row's recipe removes it: move
    // the source away (a rescan captures an existing source again), then
    // prune its captures. A later index run must not bring it back.
    if !raw_mirror_holds(&archive.data_dir, FORGOTTEN_MARKER)? {
        return Err("the raw mirror never captured the forgotten source".into());
    }
    fs::rename(&forgotten, archive.home.path().join("moved-away.jsonl"))?;
    archive.succeed(
        "mirror prune",
        &[
            "mirror",
            "prune",
            "--older-than",
            "0s",
            "--safety-hold-down",
            "0s",
            "--source-path",
            forgotten_str,
            "--apply",
            "--json",
        ],
    )?;
    archive.index(&[])?;
    if raw_mirror_holds(&archive.data_dir, FORGOTTEN_MARKER)? {
        return Err("the raw mirror still holds the forgotten source after the prune".into());
    }
    if !raw_mirror_holds(&archive.data_dir, KEPT_MARKER)? {
        return Err("a source-path mirror prune removed an unrelated capture".into());
    }
    Ok(())
}

/// Whether any raw-mirror file under `data_dir` contains `marker`.
fn raw_mirror_holds(data_dir: &Path, marker: &str) -> TestResult<bool> {
    let root = data_dir.join("raw-mirror");
    if !root.exists() {
        return Ok(false);
    }
    for entry in WalkDir::new(root) {
        let entry = entry?;
        if entry.file_type().is_file()
            && fs::read(entry.path())?
                .windows(marker.len())
                .any(|window| window == marker.as_bytes())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[test]
fn forget_apply_removes_forgotten_conversations_from_every_search_surface() -> TestResult {
    let (archive, forgotten) = indexed_archive(&["--semantic", "--embedder", "hash"])?;
    let forgotten_str = forgotten.to_str().ok_or("non-utf8 fixture path")?;
    assert_forget_clears_every_search_surface(
        &archive,
        &forgotten,
        &["--source-glob", forgotten_str],
    )
}

#[test]
fn workspace_and_age_forget_remove_conversations_from_every_search_surface() -> TestResult {
    for selectors in [
        ["--workspace-glob", "/work/scratch/*"],
        ["--older-than", "90d"],
    ] {
        let home = TempDir::new()?;
        let data_dir = home.path().join("cass-data");
        fs::create_dir_all(&data_dir)?;
        let archive = Archive { home, data_dir };
        let now = chrono::Utc::now();
        let forgotten = write_workspace_rollout(
            &archive,
            FORGOTTEN_SESSION,
            FORGOTTEN_MARKER,
            "/work/scratch/retired",
            now - chrono::Duration::days(180),
        )?;
        write_workspace_rollout(&archive, "keep-b", KEPT_MARKER, "/work/production", now)?;
        archive.index(&["--full", "--semantic", "--embedder", "hash"])?;
        assert_forget_clears_every_search_surface(&archive, &forgotten, &selectors)?;
    }
    Ok(())
}

fn assert_forget_clears_every_search_surface(
    archive: &Archive,
    forgotten: &Path,
    selectors: &[&str],
) -> TestResult {
    let forgotten_str = forgotten.to_str().ok_or("non-utf8 fixture path")?;

    // Negative controls: every surface returns the conversation before forget.
    for (surface, args) in SURFACES {
        let hits = archive.search_hits(FORGOTTEN_MARKER, args)?;
        if !hits.iter().any(|(path, _)| path == forgotten_str) {
            return Err(format!("{surface}: fixture not found before forget: {hits:?}").into());
        }
    }
    if !archive.pack(FORGOTTEN_MARKER)?.contains(FORGOTTEN_SESSION) {
        return Err("pack: fixture not found before forget".into());
    }

    let source_before = fs::read(forgotten)?;
    let assets_before = derived_assets(&archive.data_dir)?;
    let preview = archive.forget_selected(selectors, false)?;
    assert_eq!(preview["conversations_matched"], 1);
    assert_eq!(preview["conversations_deleted"], 0);
    assert_eq!(preview["dry_run"], true);
    assert_eq!(fs::read(forgotten)?, source_before);
    assert_eq!(derived_assets(&archive.data_dir)?, assets_before);

    let report = archive.forget_selected(selectors, true)?;
    if report["conversations_deleted"] != 1 {
        return Err(format!("expected one deleted conversation: {report}").into());
    }
    assert_eq!(
        fs::read(forgotten)?,
        source_before,
        "forget rewrote the source transcript"
    );

    // The semantic assets predate the deletion, so explicit semantic search
    // fails closed (exit 15) until the semantic catch-up below; every surface
    // that answers must answer without the forgotten conversation.
    let assert_forgotten_absent = |surface: &str, hits: &[(String, String)]| -> TestResult {
        match hits
            .iter()
            .find(|(path, content)| path == forgotten_str || content.contains(FORGOTTEN_MARKER))
        {
            Some(hit) => Err(format!("{surface}: forgotten conversation returned: {hit:?}").into()),
            None => Ok(()),
        }
    };
    for (surface, args) in SURFACES {
        match archive.search_hits_or_unavailable(FORGOTTEN_MARKER, args)? {
            Some(hits) => assert_forgotten_absent(surface, &hits)?,
            None if surface.starts_with("semantic") => {}
            None => return Err(format!("{surface}: unavailable after forget").into()),
        }
    }
    for surface_args in [LEXICAL, &["--mode", "hybrid", "--model", "hash"][..]] {
        if archive.search_hits(KEPT_MARKER, surface_args)?.is_empty() {
            return Err(
                format!("{surface_args:?}: forget removed the unrelated conversation").into(),
            );
        }
    }
    if archive.pack(FORGOTTEN_MARKER)?.contains(FORGOTTEN_SESSION) {
        return Err("pack still carries the forgotten conversation".into());
    }
    if archive.semantic_status_is_current()? {
        return Err("status calls the pre-forget semantic assets current".into());
    }

    // 2l1b0.78: the command the semantic error names restores every surface,
    // still without the forgotten conversation. Before the fix it took the
    // watermark skip and left semantic search unavailable forever.
    archive.index(&["--semantic", "--embedder", "hash"])?;
    for (surface, args) in SURFACES {
        assert_forgotten_absent(surface, &archive.search_hits(FORGOTTEN_MARKER, args)?)?;
        if archive.search_hits(KEPT_MARKER, args)?.is_empty() {
            return Err(format!("{surface}: the catch-up lost the unrelated conversation").into());
        }
    }
    if !archive.semantic_status_is_current()? {
        return Err("status still reports stale semantic assets after the catch-up".into());
    }

    // A conversation ingested after the forget is found on every surface, and
    // the rescan it triggers neither re-ingests the forgotten source (2l1b0.50
    // tombstones) nor lets its messages, which may reuse the freed top ids,
    // hide behind a surviving semantic watermark (2l1b0.78).
    let codex_home = archive.home.path().join(".codex");
    let late = write_codex_rollout(&codex_home, "late-c", LATE_MARKER)?;
    let late_str = late.to_str().ok_or("non-utf8 fixture path")?;
    archive.index(&["--semantic", "--embedder", "hash"])?;
    for (surface, args) in SURFACES {
        let hits = archive.search_hits(LATE_MARKER, args)?;
        if !hits.iter().any(|(path, _)| path == late_str) {
            return Err(
                format!("{surface}: a post-forget conversation is missing: {hits:?}").into(),
            );
        }
        assert_forgotten_absent(surface, &hits)?;
        assert_forgotten_absent(surface, &archive.search_hits(FORGOTTEN_MARKER, args)?)?;
    }
    Ok(())
}

#[test]
fn a_failed_lexical_purge_is_a_typed_error_and_the_next_index_completes_it() -> TestResult {
    let (archive, forgotten) = indexed_archive(&[])?;
    let forgotten_str = forgotten.to_str().ok_or("non-utf8 fixture path")?;

    let mut failing = archive.cmd();
    failing.env("CASS_TEST_FORGET_LEXICAL_REBUILD_FAILURE", "1");
    let output = archive.run_step(
        "forget --apply (injected lexical failure)",
        failing,
        &[
            "forget",
            "--source-glob",
            forgotten_str,
            "--apply",
            "--json",
        ],
    )?;
    if output.status.code() != Some(5) {
        return Err(format!("a failed purge must exit 5, got {:?}", output.status.code()).into());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value = serde_json::from_str(
        stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .ok_or("no error envelope on stderr")?,
    )?;
    if envelope["error"]["kind"] != "lexical-rebuild" {
        return Err(format!("expected kind lexical-rebuild: {envelope}").into());
    }

    // The canonical deletion stands.
    if archive.forget(forgotten_str, false)?["conversations_matched"] != 0 {
        return Err("the canonical rows must stay deleted after a failed purge".into());
    }

    // 2l1b0.79: a plain incremental `cass index` finishes the purge. Its
    // preflight sees fewer canonical conversations than the lexical
    // checkpoint was certified against and rebuilds from the canonical rows.
    // Before, it never revisited the deleted rows' documents and status
    // stayed stale with the forgotten text searchable.
    archive.index(&[])?;
    let after = archive.search_hit_paths(FORGOTTEN_MARKER)?;
    if !after.is_empty() {
        return Err(format!("the next index left the forgotten text searchable: {after:?}").into());
    }
    if archive.search_hit_paths(KEPT_MARKER)?.is_empty() {
        return Err("the repairing index lost the unrelated conversation".into());
    }
    let status = archive.succeed("status", &["status", "--json"])?;
    let status: Value = serde_json::from_slice(&status.stdout)?;
    if status["index"]["stale"] != false {
        return Err(format!(
            "lexical index still stale after the repair: {}",
            status["index"]
        )
        .into());
    }
    Ok(())
}

#[test]
fn a_failed_lexical_purge_is_repaired_by_retrying_the_same_forget() -> TestResult {
    let (archive, forgotten) = indexed_archive(&[])?;
    let forgotten_str = forgotten.to_str().ok_or("non-utf8 fixture path")?;
    let original = fs::read(&forgotten)?;
    assert!(
        archive
            .search_hit_paths(FORGOTTEN_MARKER)?
            .iter()
            .any(|path| path == forgotten_str),
        "negative control: the forgotten session must be searchable"
    );
    assert!(!archive.search_hit_paths(KEPT_MARKER)?.is_empty());

    // A fresh process must retry the derived rebuild even after the first
    // attempt deleted every canonical match. Keeping the failpoint enabled
    // for a second attempt proves this is a repair, not a successful no-op.
    for attempt in 1..=2 {
        let mut failing = archive.cmd();
        failing.env("CASS_TEST_FORGET_LEXICAL_REBUILD_FAILURE", "1");
        let output = archive.run_step(
            &format!("forget --apply (injected lexical failure, attempt {attempt})"),
            failing,
            &[
                "forget",
                "--source-glob",
                forgotten_str,
                "--apply",
                "--json",
            ],
        )?;
        assert_eq!(
            output.status.code(),
            Some(5),
            "attempt {attempt} must reach lexical rebuilding: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        let envelope: Value = serde_json::from_str(
            stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .ok_or("no error envelope on stderr")?,
        )?;
        assert_eq!(envelope["error"]["kind"], "lexical-rebuild");
        assert_eq!(
            archive.forget(forgotten_str, false)?["conversations_matched"],
            0,
            "canonical deletion must survive each failed rebuild"
        );
    }

    let repaired = archive.forget(forgotten_str, true)?;
    assert_eq!(repaired["conversations_matched"], 0);
    assert_eq!(repaired["conversations_deleted"], 0);
    assert_eq!(repaired["applied"], true);

    // Check freshness before any search can run its own recovery path.
    let status = archive.succeed("status after forget retry", &["status", "--json"])?;
    let status: Value = serde_json::from_slice(&status.stdout)?;
    assert_eq!(status["index"]["stale"], false);
    assert!(archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    assert!(!archive.search_hit_paths(KEPT_MARKER)?.is_empty());

    archive.index(&["--full"])?;
    assert!(archive.search_hit_paths(FORGOTTEN_MARKER)?.is_empty());
    assert!(!archive.search_hit_paths(KEPT_MARKER)?.is_empty());
    assert_eq!(fs::read(&forgotten)?, original);
    Ok(())
}

#[test]
fn a_forgotten_source_stays_forgotten_until_the_file_changes() -> TestResult {
    let (archive, forgotten) = indexed_archive(&[])?;
    let forgotten_str = forgotten.to_str().ok_or("non-utf8 fixture path")?;
    archive.forget(forgotten_str, true)?;

    // Neither an incremental nor a full index re-ingests an unchanged source.
    for extra in [&[][..], &["--full"][..]] {
        archive.index(extra)?;
        let hits = archive.search_hit_paths(FORGOTTEN_MARKER)?;
        if !hits.is_empty() {
            return Err(
                format!("index {extra:?} re-ingested an unchanged source: {hits:?}").into(),
            );
        }
    }

    // A new sibling session makes the connector rescan the directory and
    // re-read the forgotten, unchanged source. Before forget tombstones the
    // rescan ingested it as a new conversation again (2l1b0.50).
    let codex_home = archive.home.path().join(".codex");
    let sibling = write_codex_rollout(&codex_home, "sibling-d", SIBLING_MARKER)?;
    let sibling_str = sibling.to_str().ok_or("non-utf8 fixture path")?;
    archive.index(&[])?;
    let hits = archive.search_hit_paths(FORGOTTEN_MARKER)?;
    if !hits.is_empty() {
        return Err(format!("a sibling rescan re-ingested a forgotten source: {hits:?}").into());
    }
    if !archive
        .search_hit_paths(SIBLING_MARKER)?
        .iter()
        .any(|path| path == sibling_str)
    {
        return Err("the new sibling session was not indexed".into());
    }

    // A source that changes after forget is ingested again, whole: forget
    // removes indexed copies, it does not exclude the file (README forget row).
    let mut file = fs::OpenOptions::new().append(true).open(&forgotten)?;
    writeln!(
        file,
        r#"{{"timestamp":"2026-09-20T10:00:03.000Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"one more turn"}}]}}}}"#
    )?;
    drop(file);
    archive.index(&[])?;
    let hits = archive.search_hit_paths(FORGOTTEN_MARKER)?;
    if !hits.iter().any(|path| path == forgotten_str) {
        return Err(format!("a changed source was not re-ingested: {hits:?}").into());
    }
    Ok(())
}

/// `cass dedup --apply` deletes canonical rows too (2l1b0.78). The plain
/// index run it leads to must restore every surface, keep the canonical row,
/// and drop the collapsed twin.
#[test]
fn every_search_surface_recovers_after_dedup_apply() -> TestResult {
    use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use coding_agent_search::storage::sqlite::FrankenStorage;

    const CANONICAL_MARKER: &str = "dedupcanonicalepsilon";
    const TWIN_MARKER: &str = "deduptwinzeta";

    let (archive, _) = indexed_archive(&["--semantic", "--embedder", "hash"])?;

    // The pre-fix duplicate shape (gh #302): a bare canonical row and its
    // `projects/`-prefixed twin for one source path. Current ingest no longer
    // writes twins, so they are seeded through the storage API. Distinct
    // markers tell the two rows apart in search results.
    let source_path = archive
        .home
        .path()
        .join(".claude/projects/-proj/twin.jsonl");
    let conversation = |external_id: &str, marker: &str| Conversation {
        id: None,
        agent_slug: "claude_code".into(),
        workspace: Some(PathBuf::from("/work/dedup-test")),
        external_id: Some(external_id.to_string()),
        title: Some(format!("dedup {marker}")),
        source_path: source_path.clone(),
        started_at: Some(1_790_000_000_000),
        ended_at: Some(1_790_000_000_100),
        approx_tokens: None,
        metadata_json: Value::Null,
        messages: vec![Message {
            id: None,
            idx: 0,
            role: MessageRole::User,
            author: Some("user".into()),
            created_at: Some(1_790_000_000_010),
            content: format!("please look at {marker} again"),
            extra_json: Value::Null,
            snippets: Vec::new(),
        }],
        source_id: "local".into(),
        origin_host: None,
    };
    let storage = FrankenStorage::open(&archive.data_dir.join("agent_search.db"))?;
    let agent_id = storage.ensure_agent(&Agent {
        id: None,
        slug: "claude_code".into(),
        name: "Claude Code".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    storage.insert_conversation_tree(
        agent_id,
        None,
        &conversation("-proj/twin.jsonl", CANONICAL_MARKER),
    )?;
    storage.insert_conversation_tree(
        agent_id,
        None,
        &conversation("projects/-proj/twin.jsonl", TWIN_MARKER),
    )?;
    drop(storage);
    archive.index(&["--full", "--semantic", "--embedder", "hash"])?;

    let marker_hits = |marker: &str, args: &[&str]| -> TestResult<bool> {
        Ok(archive
            .search_hits(marker, args)?
            .iter()
            .any(|(_, content)| content.contains(marker)))
    };
    // Negative control: every surface returns both rows before the dedup.
    for (surface, args) in SURFACES {
        for marker in [CANONICAL_MARKER, TWIN_MARKER] {
            if !marker_hits(marker, args)? {
                return Err(format!("{surface}: {marker} not found before dedup").into());
            }
        }
    }

    let output = archive.succeed("dedup --apply", &["dedup", "--apply", "--json"])?;
    let report: Value = serde_json::from_slice(&output.stdout)?;
    if report["conversations_collapsed"] != 1 {
        return Err(format!("expected one collapsed twin: {report}").into());
    }
    if archive.semantic_status_is_current()? {
        return Err("status calls the pre-dedup semantic assets current".into());
    }

    archive.index(&["--semantic", "--embedder", "hash"])?;
    for (surface, args) in SURFACES {
        if marker_hits(TWIN_MARKER, args)? {
            return Err(format!("{surface}: the collapsed twin is still returned").into());
        }
        if !marker_hits(CANONICAL_MARKER, args)? {
            return Err(format!("{surface}: the canonical row was lost").into());
        }
        if archive.search_hits(KEPT_MARKER, args)?.is_empty() {
            return Err(format!("{surface}: an unrelated conversation was lost").into());
        }
    }
    if !archive.semantic_status_is_current()? {
        return Err("status still reports stale semantic assets after the index run".into());
    }
    Ok(())
}

/// The TUI and `cass serve --stdio` keep one lexical reader open across many
/// searches. A forget run while one is open must not leave the forgotten text
/// reachable through it: the TUI's reloading client drops it on its next
/// search, and a serve session, pinned until `reload` by contract
/// (docs/SEARCH_SERVICE.md), drops it on reload.
#[test]
fn search_readers_open_across_a_forget_stop_returning_forgotten_text() -> TestResult {
    use coding_agent_search::search::query::{
        FieldMask, SearchClient, SearchClientOptions, SearchFilters,
    };
    use std::io::{BufRead, BufReader};
    use std::process::{Command as StdCommand, Stdio};

    let (archive, forgotten) = indexed_archive(&[])?;
    let forgotten_str = forgotten.to_str().ok_or("non-utf8 fixture path")?;

    // The TUI's client (src/ui/app.rs open_search_service): reload on search,
    // hits hydrated against the canonical database.
    let index_path = coding_agent_search::search::tantivy::index_dir(&archive.data_dir)?;
    let db_path = archive.data_dir.join("agent_search.db");
    let client = SearchClient::open_with_options(
        &index_path,
        Some(&db_path),
        SearchClientOptions {
            enable_reload: true,
            enable_warm: true,
            strict_read_only: false,
        },
    )?
    .ok_or("the TUI client found no lexical index")?;
    let client_hits = |query: &str| -> TestResult<Vec<String>> {
        let hits: Vec<String> = client
            .search(query, SearchFilters::default(), 10, 0, FieldMask::FULL)?
            .into_iter()
            .map(|hit| hit.source_path)
            .collect();
        eprintln!(
            "{}",
            json!({"test": "cli_forget", "step": "tui client search", "query": query, "hits": hits})
        );
        Ok(hits)
    };

    let mut serve = StdCommand::new(cass_bin())
        .args(["serve", "--stdio", "--data-dir"])
        .arg(&archive.data_dir)
        .env("HOME", archive.home.path())
        .env("CASS_AUTO_REFRESH", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut serve_in = serve.stdin.take().ok_or("no serve stdin")?;
    let mut serve_out = BufReader::new(serve.stdout.take().ok_or("no serve stdout")?);
    let mut next_id = 0_u64;
    let mut serve_request = |op: &str, query: Option<&str>| -> TestResult<Vec<String>> {
        next_id += 1;
        let mut request = json!({"op": op, "id": next_id});
        if let Some(query) = query {
            request["query"] = json!(query);
            request["limit"] = json!(10);
        }
        writeln!(serve_in, "{request}")?;
        serve_in.flush()?;
        let mut line = String::new();
        serve_out.read_line(&mut line)?;
        let reply: Value = serde_json::from_str(&line)?;
        if reply["ok"] != true {
            return Err(format!("serve {request} failed: {reply}").into());
        }
        let hits: Vec<String> = reply["result"]["hits"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|hit| hit["source_path"].as_str().unwrap_or_default().to_string())
            .collect();
        eprintln!(
            "{}",
            json!({"test": "cli_forget", "step": "serve", "request": request, "hits": hits})
        );
        Ok(hits)
    };

    // Negative control: both readers find the marker before the forget.
    if !client_hits(FORGOTTEN_MARKER)?
        .iter()
        .any(|path| path == forgotten_str)
    {
        return Err("control: the TUI client did not find the marker before forget".into());
    }
    if !serve_request("search", Some(FORGOTTEN_MARKER))?
        .iter()
        .any(|path| path == forgotten_str)
    {
        return Err("control: the serve session did not find the marker before forget".into());
    }

    if archive.forget(forgotten_str, true)?["conversations_deleted"] != 1 {
        return Err("forget did not delete the fixture conversation".into());
    }
    // SearchClient rate-limits reloads to one per 300 ms.
    std::thread::sleep(std::time::Duration::from_millis(400));

    let after = client_hits(FORGOTTEN_MARKER)?;
    if !after.is_empty() {
        return Err(format!("the open TUI client still returns forgotten text: {after:?}").into());
    }
    if client_hits(KEPT_MARKER)?.is_empty() {
        return Err("the open TUI client lost the unrelated conversation".into());
    }

    // The pinned session is logged, not judged; reload is the contract.
    serve_request("search", Some(FORGOTTEN_MARKER))?;
    serve_request("reload", None)?;
    let after = serve_request("search", Some(FORGOTTEN_MARKER))?;
    if !after.is_empty() {
        return Err(format!("a reloaded serve session returns forgotten text: {after:?}").into());
    }
    if serve_request("search", Some(KEPT_MARKER))?.is_empty() {
        return Err("the reloaded serve session lost the unrelated conversation".into());
    }
    serve_request("shutdown", None)?;
    if !serve.wait()?.success() {
        return Err("cass serve did not shut down cleanly".into());
    }
    Ok(())
}

/// mxr52: before mx520 every connector scanned every remote mirror container,
/// so a synced Factory session was also archived as a Claude conversation.
/// `cass dedup` reports such rows and, with `--apply`, removes only those
/// whose file is also archived under the store's owning agent; a
/// misattributed only copy is reported and kept.
#[test]
fn dedup_removes_misattributed_remote_mirror_duplicates_only() -> TestResult {
    use coding_agent_search::franken_sync::compat::RowExt;
    use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use coding_agent_search::sources::provenance::{Source, SourceKind};
    use coding_agent_search::sources::sync::path_to_safe_dirname;
    use coding_agent_search::storage::sqlite::FrankenStorage;

    let (archive, _) = indexed_archive(&[])?;
    let mirror = archive.data_dir.join("remotes/laptop/mirror");
    let factory_container = mirror.join(path_to_safe_dirname("~/.factory/sessions"));
    let pi_container = mirror.join(path_to_safe_dirname("~/.pi/agent/sessions"));
    fs::create_dir_all(&factory_container)?;
    fs::create_dir_all(&pi_container)?;

    let storage = FrankenStorage::open(&archive.data_dir.join("agent_search.db"))?;
    storage.upsert_source(&Source {
        id: "laptop".into(),
        kind: SourceKind::Ssh,
        host_label: Some("user@laptop".into()),
        machine_id: None,
        platform: Some("linux".into()),
        config_json: Some(json!({"paths": ["~/.factory/sessions", "~/.pi/agent/sessions"]})),
        created_at: None,
        updated_at: None,
    })?;
    let seed = |slug: &str, path: &Path, external_id: &str| -> TestResult<i64> {
        let agent_id = storage.ensure_agent(&Agent {
            id: None,
            slug: slug.into(),
            name: slug.into(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let conversation = Conversation {
            id: None,
            agent_slug: slug.into(),
            workspace: None,
            external_id: Some(external_id.into()),
            title: Some(external_id.into()),
            source_path: path.to_path_buf(),
            started_at: Some(1_790_000_000_000),
            ended_at: Some(1_790_000_000_100),
            approx_tokens: None,
            metadata_json: Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_790_000_000_010),
                content: format!("mirror row {external_id}"),
                extra_json: Value::Null,
                snippets: Vec::new(),
            }],
            source_id: "laptop".into(),
            origin_host: Some("user@laptop".into()),
        };
        Ok(storage
            .insert_conversation_tree(agent_id, None, &conversation)?
            .conversation_id)
    };
    let factory_file = factory_container.join("sessions/-proj/fac.jsonl");
    let factory_row = seed("factory", &factory_file, "fac")?;
    let claude_duplicate = seed("claude_code", &factory_file, "sessions/-proj/fac.jsonl")?;
    let claude_only_copy = seed(
        "claude_code",
        &pi_container.join("sessions/--proj--/pi.jsonl"),
        "sessions/--proj--/pi.jsonl",
    )?;
    drop(storage);

    let report = |args: &[&str]| -> TestResult<Value> {
        let output = archive.succeed("dedup", args)?;
        Ok(serde_json::from_slice(&output.stdout)?)
    };
    let dry = report(&["dedup", "--json"])?;
    let section = &dry["misattributed_mirror"];
    if section["conversations_removed"] != 1 || section["unmatched"] != 1 {
        return Err(format!("unexpected dry run: {dry}").into());
    }
    let rows: Vec<(i64, Option<i64>)> = section["rows"]
        .as_array()
        .ok_or("rows missing")?
        .iter()
        .map(|row| {
            (
                row["conversation_id"].as_i64().unwrap_or(-1),
                row["owner_conversation_id"].as_i64(),
            )
        })
        .collect();
    if rows
        != [
            (claude_duplicate, Some(factory_row)),
            (claude_only_copy, None),
        ]
    {
        return Err(format!("unexpected rows: {dry}").into());
    }

    let applied = report(&["dedup", "--apply", "--json"])?;
    if applied["misattributed_mirror"]["conversations_removed"] != 1 {
        return Err(format!("unexpected apply: {applied}").into());
    }
    let storage = FrankenStorage::open_readonly(&archive.data_dir.join("agent_search.db"))?;
    let laptop_rows: Vec<i64> = storage
        .raw()
        .query("SELECT id FROM conversations WHERE source_id = 'laptop' ORDER BY id")?
        .into_iter()
        .map(|row| row.get_typed::<i64>(0))
        .collect::<Result<_, _>>()?;
    drop(storage);
    if laptop_rows != [factory_row, claude_only_copy] {
        return Err(format!("expected only the duplicate removed: {laptop_rows:?}").into());
    }

    let again = report(&["dedup", "--apply", "--json"])?;
    if again["misattributed_mirror"]["conversations_removed"] != 0
        || again["misattributed_mirror"]["unmatched"] != 1
    {
        return Err(format!("dedup is not idempotent: {again}").into());
    }
    Ok(())
}
