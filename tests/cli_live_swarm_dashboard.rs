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
        let output = Command::new("git").arg("-C").arg(&repo)
            .args(["init", "--quiet", "--initial-branch=main"]).output()?;
        require_success(&output)?;
        Ok(Self { _temp: temp, repo, nested, home, data })
    }

    fn write_beads(&self, body: &str) -> std::io::Result<PathBuf> {
        let directory = self.repo.join(".beads");
        fs::create_dir_all(&directory)?;
        let path = directory.join("issues.jsonl");
        fs::write(&path, body)?;
        Ok(path)
    }

    fn run(&self, args: &[&str]) -> Result<Output, Box<dyn Error>> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
        for (name, _) in std::env::vars_os() {
            let key = name.to_string_lossy();
            if ["CASS_", "TOON_", "FRANKENSEARCH_", "FSQLITE_", "GIT_"].iter().any(|prefix| key.starts_with(prefix)) {
                command.env_remove(name);
            }
        }
        let output = command.current_dir(&self.nested)
            .env("HOME", &self.home).env("USERPROFILE", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("CASS_DATA_DIR", &self.data)
            .env("CASS_AUTO_REFRESH", "0")
            .env("CASS_SEMANTIC_ENABLED", "0")
            .args(["--data-dir"]).arg(&self.data)
            .args(["swarm", "dashboard"]).args(args).output()?;
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
            "command failed with {}; stderr={}", output.status,
            String::from_utf8_lossy(&output.stderr)
        )).into())
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
    assert_eq!(repository["beads"]["active_tasks"].as_array().unwrap().len(), 2);
    assert_eq!(repository["beads"]["attention_tasks"][0]["id"], "blocked");
    assert_eq!(repository["git"]["provider"]["status"], "partial");
    assert!(repository["git"]["unstaged_changes"].is_null());
    assert_eq!(repository["coordination_verified"], false);
    assert!(!output.to_string().contains("SECRET-DESCRIPTION-NOT-PUBLISHED"));
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
