//! `cass forget --apply` must stop every search surface from returning the
//! forgotten conversations (bead coding_agent_session_search-2l1b0.50).
//!
//! Before the fix, forget deleted the canonical rows and rebuilt FTS and
//! analytics, but left the Quill lexical generation untouched. Quill documents
//! store message content, so a plain search kept returning the forgotten text
//! until some later `cass index` rebuilt the index.
//!
//! Forget does not rewrite semantic vectors. The semantic assets no longer
//! match the database afterwards, so explicit semantic search fails closed
//! (`semantic-unavailable`), and every semantic hit is hydrated from its
//! canonical row anyway; the surface test proves no surface leaks.
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
    Ok(())
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
    // may fail closed (exit 15); every surface that answers must answer
    // without the forgotten conversation. Restoring semantic search after a
    // deletion is tracked separately: `cass index --semantic` does not.
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
    Ok(())
}

#[test]
fn a_failed_lexical_purge_is_a_typed_error_and_index_full_completes_it() -> TestResult {
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

    // The hinted `cass index --full` finishes the purge from the canonical
    // rows (an incremental run never revisits deleted rows' documents).
    archive.index(&["--full"])?;
    let after = archive.search_hit_paths(FORGOTTEN_MARKER)?;
    if !after.is_empty() {
        return Err(format!("index --full left the forgotten text searchable: {after:?}").into());
    }
    if archive.search_hit_paths(KEPT_MARKER)?.is_empty() {
        return Err("the repairing index lost the unrelated conversation".into());
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
