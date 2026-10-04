//! Live-binary regressions for repository-local dashboard collection.
//! Run through the repository's RCH gate; these tests require the built cass
//! binary and Git. They do not download models or inspect a user's archive.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn Error>>;

struct Fixture {
    _temp: tempfile::TempDir,
    repo: PathBuf,
    nested: PathBuf,
    home: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repo");
        let nested = repo.join("nested/inside");
        let home = temp.path().join("home");
        let data = temp.path().join("data");
        for directory in [&nested, &home, &data] {
            fs::create_dir_all(directory)?;
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "--quiet", "--initial-branch=main"])
            .output()?;
        require_success(&output)?;
        Ok(Self {
            _temp: temp,
            repo,
            nested,
            home,
            data,
        })
    }

    fn write_beads(&self, body: &str) -> std::io::Result<PathBuf> {
        let directory = self.repo.join(".beads");
        fs::create_dir_all(&directory)?;
        let path = directory.join("issues.jsonl");
        fs::write(&path, body)?;
        Ok(path)
    }

    fn run(&self, args: &[&str]) -> Result<Output, Box<dyn Error>> {
        self.run_swarm(&self.nested, "dashboard", args)
    }

    fn run_swarm(
        &self,
        cwd: &Path,
        subcommand: &str,
        args: &[&str],
    ) -> Result<Output, Box<dyn Error>> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
        for (name, _) in std::env::vars_os() {
            let key = name.to_string_lossy();
            if ["CASS_", "TOON_", "FRANKENSEARCH_", "FSQLITE_", "GIT_"]
                .iter()
                .any(|prefix| key.starts_with(prefix))
            {
                command.env_remove(name);
            }
        }
        let output = command
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("CASS_DATA_DIR", &self.data)
            .env("CASS_AUTO_REFRESH", "0")
            .env("CASS_SEMANTIC_ENABLED", "0")
            // `--data-dir` is not a global flag (swarm commands take the data
            // dir from CASS_DATA_DIR above; see 0bef5fa7 for the sibling harness).
            .args(["swarm", subcommand])
            .args(args)
            .output()?;
        require_success(&output)?;
        Ok(output)
    }

    fn json(&self) -> Result<Value, Box<dyn Error>> {
        Ok(serde_json::from_slice(&self.run(&["--json"])?.stdout)?)
    }
}

fn require_success(output: &Output) -> TestResult {
    if output.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "command failed with {}; stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
        .into())
    }
}

fn fixture_body() -> String {
    [
        json!({"id":"done", "status":"closed", "issue_type":"task", "priority":0}),
        json!({"id":"ready", "status":"open", "issue_type":"task", "priority":0,
            "title":"Implement <script>not executable</script>",
            "description":"SECRET-DESCRIPTION-NOT-PUBLISHED",
            "dependencies":[{"depends_on_id":"done", "type":"blocks"}]}),
        json!({"id":"blocked", "status":"open", "issue_type":"task", "priority":0,
            "dependencies":[{"depends_on_id":"doing", "type":"blocks"}]}),
        json!({"id":"doing", "status":"in_progress", "issue_type":"task", "priority":1}),
        json!({"id":"assigned", "status":"open", "issue_type":"task", "priority":1, "assignee":"BlueAgent"}),
        json!({"id":"epic", "status":"open", "issue_type":"epic", "priority":0}),
    ].iter().map(Value::to_string).collect::<Vec<_>>().join("\n")
}

fn assert_preserved(path: &Path, before: &[u8], modified: std::time::SystemTime) -> TestResult {
    assert_eq!(fs::read(path)?, before);
    assert_eq!(fs::metadata(path)?.modified()?, modified);
    Ok(())
}

#[test]
fn nested_live_dashboard_classifies_tasks_without_mutating_beads() -> TestResult {
    let fixture = Fixture::new()?;
    let path = fixture.write_beads(&fixture_body())?;
    let before = fs::read(&path)?;
    let modified = fs::metadata(&path)?.modified()?;
    let output = fixture.json()?;
    let repository = &output["cards"]["repository"];
    assert_eq!(output["_meta"]["source"], "live");
    assert_eq!(repository["beads"]["provider"]["status"], "ok");
    assert_eq!(repository["beads"]["complete"], true);
    assert_eq!(repository["beads"]["locally_unblocked_count"], 1);
    assert_eq!(repository["beads"]["candidate_tasks"][0]["id"], "ready");
    assert_eq!(
        repository["beads"]["active_tasks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(repository["beads"]["attention_tasks"][0]["id"], "blocked");
    assert_eq!(repository["git"]["provider"]["status"], "partial");
    assert!(repository["git"]["unstaged_changes"].is_null());
    assert_eq!(repository["coordination_verified"], false);
    assert!(
        !output
            .to_string()
            .contains("SECRET-DESCRIPTION-NOT-PUBLISHED")
    );
    assert_preserved(&path, &before, modified)
}

/// `swarm lint`, `evidence`, `proof-debt` and `failure-patterns` read the same
/// bounded live providers as `swarm status`, from the repository root (the
/// live collector's precondition). They used to report every provider as
/// `live-provider-unimplemented` and find nothing outside fixtures, so the git
/// provider was never read. The Beads provider shells out to `br`; where `br`
/// is installed, an in-progress bead with no Agent Mail thread is a finding.
#[test]
fn live_swarm_lint_reads_repository_providers_and_finds_bead_gaps() -> TestResult {
    let fixture = Fixture::new()?;
    let path = fixture.write_beads(&fixture_body())?;
    let before = fs::read(&path)?;
    let modified = fs::metadata(&path)?.modified()?;

    let lint: Value = serde_json::from_slice(
        &fixture
            .run_swarm(&fixture.repo, "lint", &["--json"])?
            .stdout,
    )?;
    let providers = lint["providers"].as_array().ok_or("providers array")?;
    assert!(
        providers
            .iter()
            .all(|provider| provider["error_kind"] != "live-provider-unimplemented"),
        "no live provider may be a placeholder: {providers:?}"
    );
    let provider = |name: &str| {
        providers
            .iter()
            .find(|provider| provider["name"] == name)
            .ok_or(format!("{name} provider"))
    };
    let git = provider("git")?;
    assert!(
        git["status"] == "partial" || git["status"] == "ok",
        "the repository's git state must be read live: {git}"
    );
    let beads = provider("beads")?;
    let beads_read = beads["status"] == "partial" || beads["status"] == "ok";
    assert!(
        beads_read || beads["error_kind"] == "live-provider-read-failed",
        "the Beads provider must attempt a real read: {beads}"
    );
    if beads_read {
        let findings = lint["findings"].as_array().ok_or("findings array")?;
        assert!(
            findings
                .iter()
                .any(|finding| finding["code"] == "missing-start-mail"
                    && finding["subject_id"] == "doing"),
            "the in-progress bead without a start mail must be flagged: {findings:?}"
        );
    }
    assert_eq!(lint["mutation_contract"]["read_only"], true);

    for subcommand in ["evidence", "proof-debt", "failure-patterns"] {
        let payload: Value = serde_json::from_slice(
            &fixture
                .run_swarm(&fixture.repo, subcommand, &["--json"])?
                .stdout,
        )?;
        assert!(
            !payload.to_string().contains("live-provider-unimplemented"),
            "swarm {subcommand} still reports placeholder providers: {payload}"
        );
    }
    assert!(
        !lint
            .to_string()
            .contains("SECRET-DESCRIPTION-NOT-PUBLISHED")
    );
    assert_preserved(&path, &before, modified)
}

#[test]
fn live_html_has_task_cockpit_and_explicit_uncollected_sources() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write_beads(&fixture_body())?;
    let html = String::from_utf8(fixture.run(&["--html"])?.stdout)?;
    assert!(html.contains("Repository and task cockpit"));
    assert!(html.contains("Locally unblocked candidates"));
    assert!(html.contains("Uncollected sources"));
    assert!(html.contains("Confirm reservations before starting work"));
    assert!(!html.contains("<script>"));
    assert!(!html.contains("SECRET-DESCRIPTION-NOT-PUBLISHED"));
    Ok(())
}

#[test]
fn missing_snapshot_is_unavailable_not_a_zero_task_success() -> TestResult {
    let fixture = Fixture::new()?;
    let output = fixture.json()?;
    let beads = &output["cards"]["repository"]["beads"];
    assert_eq!(beads["provider"]["status"], "unavailable");
    assert_eq!(beads["complete"], false);
    assert!(beads["counts"]["open"].is_null());
    assert!(beads["locally_unblocked_count"].is_null());
    assert!(!fixture.repo.join(".beads").exists());
    Ok(())
}

#[test]
fn malformed_snapshot_never_authorizes_starting_a_valid_prefix_task() -> TestResult {
    let fixture = Fixture::new()?;
    fixture.write_beads(&format!("{}\n{{malformed-record}}\n", fixture_body()))?;
    let output = fixture.json()?;
    let beads = &output["cards"]["repository"]["beads"];
    assert_eq!(beads["provider"]["status"], "partial");
    assert_eq!(beads["complete"], false);
    assert!(beads["candidate_tasks"].as_array().unwrap().is_empty());
    assert!(beads["locally_unblocked_count"].is_null());
    Ok(())
}
