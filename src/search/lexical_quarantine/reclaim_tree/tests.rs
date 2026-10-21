use super::*;
use super::super::{Limits, inspect_at};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;
use std::time::Duration;

struct Fixture {
    temp: tempfile::TempDir,
    _lock: File,
    target: PathBuf,
    now: SystemTime,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let lock = fs::OpenOptions::new().create(true).truncate(false)
            .read(true).write(true).open(temp.path().join("index-run.lock")).unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        let target = temp.path().join(".lexical-rebuild-quarantine-000001");
        fs::create_dir_all(target.join("index/nested")).unwrap();
        fs::write(target.join("index/a"), b"first").unwrap();
        fs::write(target.join("index/nested/z"), b"last").unwrap();
        Self { temp, _lock: lock, target, now: SystemTime::now() + Duration::from_secs(86400) }
    }

    fn capture(&self) -> (Plan, Budget) {
        let inventory = inspect_at(self.temp.path(), self.now, true, Limits::default()).unwrap();
        assert!(inventory.complete, "{:?}", inventory.warnings);
        let mut budget = Budget::new(Limits::default());
        let (_, plan) = Plan::observe(&inventory.artifacts[0], self.now, &mut budget).unwrap();
        (plan, budget)
    }

    fn remove(&self, plan: &Plan, budget: &mut Budget) -> io::Result<()> {
        let owner = same_file::Handle::from_path(self.temp.path())?;
        plan.remove(&self.target, false, &owner, budget)
    }
}

#[test]
fn nested_observed_tree_is_removed_without_enumerating_new_targets() {
    let fixture = Fixture::new();
    let (plan, mut budget) = fixture.capture();
    assert_eq!(plan.nodes.len(), 5);
    fixture.remove(&plan, &mut budget).unwrap();
    assert!(!fixture.target.exists());
    assert_eq!(budget.removal_entries, 5);
    assert!(fixture.temp.path().join("index-run.lock").exists());
}

#[test]
fn new_unobserved_files_stop_empty_directory_removal_and_survive() {
    let fixture = Fixture::new();
    let (plan, mut budget) = fixture.capture();
    let extra = fixture.target.join("index/nested/operator-data");
    fs::write(&extra, b"not approved").unwrap();
    assert!(fixture.remove(&plan, &mut budget).is_err());
    assert_eq!(fs::read(extra).unwrap(), b"not approved");
    assert!(!fixture.target.join("index/nested/z").exists());
    assert!(fixture.target.join("index/a").exists(), "stop after the first refusal");
}

#[test]
fn a_replaced_payload_symlink_is_not_unlinked_or_followed() {
    let fixture = Fixture::new();
    let (plan, mut budget) = fixture.capture();
    let outside = fixture.temp.path().join("outside");
    fs::write(&outside, b"outside sentinel").unwrap();
    let payload = fixture.target.join("index/nested/z");
    fs::rename(&payload, fixture.temp.path().join("saved-payload")).unwrap();
    symlink(&outside, &payload).unwrap();
    assert!(fixture.remove(&plan, &mut budget).is_err());
    assert_eq!(budget.removal_entries, 0);
    assert!(fs::symlink_metadata(payload).unwrap().file_type().is_symlink());
    assert_eq!(fs::read(outside).unwrap(), b"outside sentinel");
}

#[test]
fn replacement_ancestor_symlink_cannot_redirect_a_payload_unlink() {
    let fixture = Fixture::new();
    let (plan, mut budget) = fixture.capture();
    let outside = fixture.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("z"), b"outside sentinel").unwrap();
    let nested = fixture.target.join("index/nested");
    fs::rename(&nested, fixture.temp.path().join("saved-directory")).unwrap();
    symlink(&outside, &nested).unwrap();
    assert!(fixture.remove(&plan, &mut budget).is_err());
    assert_eq!(budget.removal_entries, 0);
    assert_eq!(fs::read(outside.join("z")).unwrap(), b"outside sentinel");
    assert_eq!(fs::read(fixture.temp.path().join("saved-directory/z")).unwrap(), b"last");
}

#[test]
fn changed_payload_and_new_hard_link_are_refused_after_capture() {
    for hard_link in [false, true] {
        let fixture = Fixture::new();
        let (plan, mut budget) = fixture.capture();
        let payload = fixture.target.join("index/nested/z");
        if hard_link {
            fs::hard_link(&payload, fixture.temp.path().join("outside-link")).unwrap();
        } else {
            fs::write(&payload, b"changed content").unwrap();
        }
        assert!(fixture.remove(&plan, &mut budget).is_err());
        assert_eq!(budget.removal_entries, 0);
        assert!(payload.exists());
    }
}

#[test]
fn replacement_root_and_parent_are_refused_before_unlink() {
    let fixture = Fixture::new();
    let (plan, mut budget) = fixture.capture();
    let saved = fixture.temp.path().join("saved-root");
    fs::rename(&fixture.target, &saved).unwrap();
    fs::create_dir_all(fixture.target.join("index/nested")).unwrap();
    fs::write(fixture.target.join("index/nested/z"), b"replacement").unwrap();
    assert!(fixture.remove(&plan, &mut budget).is_err());
    assert_eq!(budget.removal_entries, 0);
    assert_eq!(fs::read(saved.join("index/nested/z")).unwrap(), b"last");
    assert_eq!(fs::read(fixture.target.join("index/nested/z")).unwrap(), b"replacement");
    let unrelated = tempfile::tempdir().unwrap();
    let wrong_owner = same_file::Handle::from_path(unrelated.path()).unwrap();
    assert!(plan.remove(&fixture.target, false, &wrong_owner, &mut budget).is_err());
    assert_eq!(budget.removal_entries, 0);
}

#[test]
fn exhausted_budget_and_deadline_never_start_an_unlink() {
    let fixture = Fixture::new();
    let (plan, mut budget) = fixture.capture();
    budget.removal_entries = budget.limits.tree_entries;
    assert!(fixture.remove(&plan, &mut budget).unwrap_err().to_string().contains("removal-entry"));
    budget.removal_entries = 0;
    budget.limits.timeout = Duration::ZERO;
    assert!(fixture.remove(&plan, &mut budget).is_err());
    assert_eq!(budget.removal_entries, 0);
    assert_eq!(fs::read(fixture.target.join("index/nested/z")).unwrap(), b"last");
}

#[test]
fn removal_budget_is_shared_and_partial_work_requires_a_fresh_observation() {
    let fixture = Fixture::new();
    let (plan, mut budget) = fixture.capture();
    budget.removal_entries = budget.limits.tree_entries - 1;
    assert!(fixture.remove(&plan, &mut budget).unwrap_err().to_string().contains("removal-entry"));
    assert!(!fixture.target.join("index/nested/z").exists());
    assert!(fixture.target.join("index/nested").exists());
    // The spent sweep allowance is not reset for another call.
    assert!(fixture.remove(&plan, &mut budget).is_err());
    assert_eq!(budget.removal_entries, budget.limits.tree_entries);
    let (fresh, mut fresh_budget) = fixture.capture();
    assert!(fixture.remove(&fresh, &mut budget).unwrap_err().to_string().contains("removal-entry"));
    assert!(fixture.target.join("index/a").exists());
    fixture.remove(&fresh, &mut fresh_budget).unwrap();
    assert!(!fixture.target.exists());
}

#[test]
fn non_utf8_names_are_removed_by_exact_bytes_and_traversal_names_are_refused() {
    let fixture = Fixture::new();
    let name = std::ffi::OsString::from_vec(b"raw-\xff\n".to_vec());
    fs::write(fixture.target.join("index").join(name), b"bytes").unwrap();
    let (plan, mut budget) = fixture.capture();
    fixture.remove(&plan, &mut budget).unwrap();
    assert!(!fixture.target.exists());
    for name in ["", ".", "..", "../outside", "/absolute", "a/b", "bad\0name"] {
        assert!(sys::name(OsStr::new(name)).is_err());
    }
}

#[test]
fn a_replaced_special_file_is_observed_without_opening_its_payload() {
    let fixture = Fixture::new();
    let (plan, mut budget) = fixture.capture();
    let path = fixture.target.join("index/nested/z");
    fs::rename(&path, fixture.temp.path().join("saved-payload")).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
    assert!(fixture.remove(&plan, &mut budget).is_err());
    assert_eq!(budget.removal_entries, 0);
    assert!(fs::symlink_metadata(path).is_ok());
}
