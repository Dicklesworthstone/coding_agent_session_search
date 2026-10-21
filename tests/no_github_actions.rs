//! GitHub Actions is permanently off (owner, 2026-10-09: "NO GH ACTIONS,
//! PERIOD!!! EVER!!! we ONLY ever use /dsr"). Verification runs through
//! `scripts/gate.sh` on the rch fleet; Windows/macOS proof and releases go
//! through dsr hosts. Workflow files kept arriving after that rule anyway
//! (21 commits by 2026-10-21), and finished fixes parked in them for a job
//! that never runs sat unapplied on main (bead nqbwq). These tests fail if a
//! workflow reappears or the UBS pin leaves the place the gate reads it.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn no_github_actions_workflows_exist() {
    let dir = repo_root().join(".github").join("workflows");
    let present: Vec<String> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .map(|entry| {
                entry
                    .unwrap_or_else(|err| panic!("read {}: {err}", dir.display()))
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => panic!("read {}: {err}", dir.display()),
    };
    assert!(
        present.is_empty(),
        "GitHub Actions is off: verify with scripts/gate.sh, release with dsr; remove {present:?} from .github/workflows"
    );
}

#[test]
fn ubs_version_pin_is_where_the_gate_reads_it() {
    let pin = std::fs::read_to_string(repo_root().join("scripts").join("ubs-version.txt"))
        .expect("scripts/ubs-version.txt pins the UBS release the gate runs");
    assert!(
        !pin.trim().is_empty(),
        "scripts/ubs-version.txt must name a UBS release (got {pin:?})"
    );
    let gate = std::fs::read_to_string(repo_root().join("scripts").join("gate.sh"))
        .expect("scripts/gate.sh");
    assert!(
        gate.contains("< scripts/ubs-version.txt"),
        "scripts/gate.sh must read the UBS pin from scripts/ubs-version.txt"
    );
}
