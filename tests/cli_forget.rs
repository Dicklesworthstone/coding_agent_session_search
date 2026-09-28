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
            .env("CASS_DATA_DIR", &self.data_dir)
            .env("CASS_AUTO_REFRESH", "0")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
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
        let mut args = vec!["forget", "--source-glob", source_glob, "--json"];
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

    let report = archive.forget(forgotten_str, true)?;
    if report["conversations_deleted"] != 1 {
        return Err(format!("expected one deleted conversation: {report}").into());
    }

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
