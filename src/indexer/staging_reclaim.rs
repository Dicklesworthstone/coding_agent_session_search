//! Reclamation of orphaned lexical staging directories (GH #324).
//!
//! The staged lexical rebuild pipeline builds shards and merges them inside
//! `tempfile::TempDir`s created next to the versioned index directory:
//! `<data_dir>/index/cass-lexical-shards.<rand>` and
//! `<data_dir>/index/cass-lexical-merge.<rand>`, plus
//! `cass-federated-materialize-<rand>` (federated bundle materialization)
//! and `cass-empty-lexical-repair-<rand>` (empty-DB repair path). A clean
//! `Drop` removes them, but any hard exit (panic, `process::exit`, SIGKILL,
//! OOM) strands them on disk. Under a supervisor restart loop the strands
//! accumulate without bound — the #324 incident stranded ~410 GB of staging
//! debris in one night and then wedged the service hard-down behind its own
//! disk-headroom preflight (exit 14).
//!
//! This module deletes provably-orphaned staging directories at indexer
//! startup, BEFORE the disk-headroom preflight, so a crash loop self-heals
//! instead of converting into a disk-full outage. Safety invariants:
//!
//! 1. **Exclusive lock**: callers must hold the `index-run.lock` exclusive
//!    flock for the data dir (see
//!    [`reclaim_orphaned_staging_dirs_under_index_run_lock`]). Lexical
//!    staging dirs are only ever created by an indexer holding that lock,
//!    and flocks are released by the kernel on any process death (including
//!    SIGKILL), so while we hold it every existing lexical staging dir is
//!    orphaned by construction.
//! 2. **Name allowlist**: only DIRECT children of the staging root whose
//!    names extend a known machine-generated staging prefix are eligible.
//!    Nothing recursive, nothing user-named — `v6`/`v8`, `agent_search.db`,
//!    publish backups, and quarantine evidence can never match.
//! 3. **Type check**: the entry must be a real directory. Symlinks are
//!    skipped outright (`symlink_metadata`), and `remove_dir_all` does not
//!    follow symlinks inside the tree, so a hostile or accidental link can
//!    never redirect the sweep outside the staging root.
//! 4. **Age threshold**: entries modified too recently are skipped as
//!    defense-in-depth against clock skew and any future code path that
//!    creates a matching dir without holding the lock. Federated
//!    materialize dirs use a longer threshold because the short-lived
//!    write-path materialization is not provably serialized by the
//!    index-run lock at every call site. A skipped-young orphan is bounded
//!    debris: the next startup reclaims it.
//! 5. **Rename-then-delete**: an eligible dir is first atomically renamed
//!    to `<name>.reclaim-<pid>-<seq>` (same parent, same filesystem), then
//!    deleted. If deletion is interrupted the marker name still extends the
//!    original allowlisted prefix, so the next sweep retries it, and a
//!    half-deleted tree can never be mistaken for a live staging dir.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Staging dir prefixes that are only ever created while holding the
/// exclusive `index-run.lock`, and are therefore provably orphaned whenever
/// the sweep (which also holds that lock) observes them.
const LOCK_SERIALIZED_STAGING_PREFIXES: &[&str] = &[
    "cass-lexical-shards.",
    "cass-lexical-merge.",
    "cass-empty-lexical-repair-",
];

/// Staging dir prefixes created by write-path index opens that are not
/// provably serialized by the index-run lock at every call site; these only
/// become eligible after a conservative age threshold.
const SHARED_STAGING_PREFIXES: &[&str] = &["cass-federated-materialize-"];

/// Minimum age (by directory mtime) before a lock-serialized staging dir is
/// reclaimed. The lock alone already proves orphanhood; this is
/// defense-in-depth against clock skew and future unlocked creators.
const LOCK_SERIALIZED_STAGING_MIN_AGE: Duration = Duration::from_secs(60);

/// Minimum age (by directory mtime) before a shared staging dir is
/// reclaimed. Federated materialization runs for seconds-to-minutes; an
/// hour-old materialize dir is debris.
const SHARED_STAGING_MIN_AGE: Duration = Duration::from_secs(60 * 60);

/// Infix appended (with pid+seq) when renaming a dir for deletion. Kept as
/// part of the reclaim allowlist so interrupted deletions are retried.
const RECLAIM_MARKER_INFIX: &str = ".reclaim-";

static RECLAIM_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Default)]
pub(crate) struct StagingReclaimReport {
    /// Orphaned staging directories fully deleted.
    pub reclaimed_dirs: usize,
    /// Approximate bytes freed (file sizes summed before deletion).
    pub reclaimed_bytes: u64,
    /// Matching directories skipped because they were modified too recently.
    pub skipped_young: usize,
    /// Non-fatal errors encountered while sweeping (kept for logging; a
    /// failed reclaim is retried on the next startup).
    pub errors: Vec<String>,
}

impl StagingReclaimReport {
    fn merge(&mut self, other: StagingReclaimReport) {
        self.reclaimed_dirs += other.reclaimed_dirs;
        self.reclaimed_bytes = self.reclaimed_bytes.saturating_add(other.reclaimed_bytes);
        self.skipped_young += other.skipped_young;
        self.errors.extend(other.errors);
    }

    /// Emit a tracing summary. Quiet when the sweep found nothing.
    pub(crate) fn log(&self) {
        if self.reclaimed_dirs > 0 || self.reclaimed_bytes > 0 {
            tracing::info!(
                reclaimed_dirs = self.reclaimed_dirs,
                reclaimed_bytes = self.reclaimed_bytes,
                skipped_young = self.skipped_young,
                "reclaimed orphaned lexical staging directories from previous crashed runs (#324)"
            );
        }
        for error in &self.errors {
            tracing::warn!(
                error = %error,
                "orphaned staging dir sweep hit a non-fatal error; will retry next startup"
            );
        }
    }
}

/// Classification of a directory-entry name against the staging allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StagingNameClass {
    /// Not a staging dir name; never touched.
    NotStaging,
    /// Created only under the index-run lock.
    LockSerialized,
    /// Created by write paths not provably under the index-run lock.
    Shared,
}

/// Match a direct-child name against the staging allowlist. The name must
/// STRICTLY extend one of the known prefixes (tempfile always appends a
/// random suffix, so a bare prefix is not a name cass ever creates).
fn classify_staging_name(name: &str) -> StagingNameClass {
    for prefix in LOCK_SERIALIZED_STAGING_PREFIXES {
        if name.len() > prefix.len() && name.starts_with(prefix) {
            return StagingNameClass::LockSerialized;
        }
    }
    for prefix in SHARED_STAGING_PREFIXES {
        if name.len() > prefix.len() && name.starts_with(prefix) {
            return StagingNameClass::Shared;
        }
    }
    StagingNameClass::NotStaging
}

/// True when `name` is a rename marker left behind by an interrupted
/// deletion (see safety invariant 5). Marker names still extend the original
/// staging prefix, so `classify_staging_name` has already allowlisted them.
fn is_reclaim_marker(name: &str) -> bool {
    name.contains(RECLAIM_MARKER_INFIX)
}

fn dir_tree_size_bytes(root: &Path) -> u64 {
    let mut total = 0_u64;
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let Ok(entry) = entry else { continue };
        let Ok(metadata) = entry.path().symlink_metadata() else {
            continue;
        };
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    total
}

/// Sweep the staging root(s) for a data dir. `data_dir` itself is also swept
/// (same allowlist) to cover historical layouts that staged directly under
/// the data dir.
///
/// # Lock requirement
///
/// The caller MUST hold the exclusive `index-run.lock` for `data_dir` (see
/// module docs, invariant 1).
pub(crate) fn reclaim_orphaned_staging_dirs_for_data_dir(
    data_dir: &Path,
    now: SystemTime,
) -> StagingReclaimReport {
    let staging_root = crate::search::tantivy::expected_index_dir(data_dir)
        .parent()
        .map_or_else(|| data_dir.join("index"), Path::to_path_buf);

    let mut report = reclaim_orphaned_staging_dirs_under_index_run_lock(&staging_root, now);
    if staging_root != *data_dir {
        report.merge(reclaim_orphaned_staging_dirs_under_index_run_lock(
            data_dir, now,
        ));
    }
    report
}

/// Sweep exactly one directory's DIRECT children for orphaned staging dirs.
///
/// # Lock requirement
///
/// The caller MUST hold the exclusive `index-run.lock` for the data dir that
/// owns `staging_root`. That lock is what proves lock-serialized staging
/// dirs are orphaned rather than owned by a live indexer.
pub(crate) fn reclaim_orphaned_staging_dirs_under_index_run_lock(
    staging_root: &Path,
    now: SystemTime,
) -> StagingReclaimReport {
    let mut report = StagingReclaimReport::default();
    let entries = match fs::read_dir(staging_root) {
        Ok(entries) => entries,
        // A missing staging root simply means nothing was ever staged.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return report,
        Err(err) => {
            report
                .errors
                .push(format!("reading {}: {err}", staging_root.display()));
            return report;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                report
                    .errors
                    .push(format!("listing {}: {err}", staging_root.display()));
                continue;
            }
        };
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        let class = classify_staging_name(name);
        if class == StagingNameClass::NotStaging {
            continue;
        }
        let path = entry.path();
        // symlink_metadata: never follow a link that merely LOOKS like a
        // staging dir (invariant 3).
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) => {
                // Raced away (e.g. our own earlier rename); nothing to do.
                if err.kind() != std::io::ErrorKind::NotFound {
                    report
                        .errors
                        .push(format!("stat {}: {err}", path.display()));
                }
                continue;
            }
        };
        if !metadata.is_dir() {
            continue;
        }

        let min_age = match class {
            StagingNameClass::LockSerialized => LOCK_SERIALIZED_STAGING_MIN_AGE,
            StagingNameClass::Shared => SHARED_STAGING_MIN_AGE,
            StagingNameClass::NotStaging => unreachable!("filtered above"),
        };
        let old_enough = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= min_age);
        if !old_enough {
            // Includes unreadable and future mtimes: fail closed, retry on a
            // later startup once the age can be proven.
            report.skipped_young += 1;
            continue;
        }

        let reclaimed_bytes = dir_tree_size_bytes(&path);
        let doomed_path = if is_reclaim_marker(name) {
            // Already renamed by an interrupted earlier sweep; just finish
            // the deletion.
            path.clone()
        } else {
            let marker_name = format!(
                "{name}{RECLAIM_MARKER_INFIX}{}-{}",
                std::process::id(),
                RECLAIM_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            let marker_path: PathBuf = staging_root.join(marker_name);
            match fs::rename(&path, &marker_path) {
                Ok(()) => marker_path,
                Err(err) => {
                    if err.kind() != std::io::ErrorKind::NotFound {
                        report.errors.push(format!(
                            "renaming {} for reclamation: {err}",
                            path.display()
                        ));
                    }
                    continue;
                }
            }
        };
        match fs::remove_dir_all(&doomed_path) {
            Ok(()) => {
                report.reclaimed_dirs += 1;
                report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(reclaimed_bytes);
            }
            Err(err) => {
                report.errors.push(format!(
                    "removing orphaned staging dir {}: {err}",
                    doomed_path.display()
                ));
            }
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    fn make_dir_with_payload(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("nested").join("payload.bin"), vec![0_u8; 4096]).unwrap();
        dir
    }

    /// `now` shifted far enough forward that any dir created during the test
    /// exceeds the given threshold.
    fn future_now(min_age: Duration) -> SystemTime {
        SystemTime::now() + min_age + Duration::from_secs(600)
    }

    #[test]
    fn stale_lexical_staging_dirs_are_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let shards = make_dir_with_payload(tmp.path(), "cass-lexical-shards.abc123");
        let merge = make_dir_with_payload(tmp.path(), "cass-lexical-merge.xYz789");
        let repair = make_dir_with_payload(tmp.path(), "cass-empty-lexical-repair-q1w2e3");

        let report = reclaim_orphaned_staging_dirs_under_index_run_lock(
            tmp.path(),
            future_now(LOCK_SERIALIZED_STAGING_MIN_AGE),
        );

        assert_eq!(report.reclaimed_dirs, 3, "errors: {:?}", report.errors);
        assert!(report.reclaimed_bytes >= 3 * 4096);
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        assert!(!shards.exists());
        assert!(!merge.exists());
        assert!(!repair.exists());
    }

    #[test]
    fn fresh_lexical_staging_dirs_survive_the_sweep() {
        // A dir modified moments ago is treated as potentially live even
        // though the lock argument says otherwise — invariant 4.
        let tmp = tempfile::tempdir().unwrap();
        let fresh = make_dir_with_payload(tmp.path(), "cass-lexical-merge.fresh1");

        let report =
            reclaim_orphaned_staging_dirs_under_index_run_lock(tmp.path(), SystemTime::now());

        assert_eq!(report.reclaimed_dirs, 0);
        assert_eq!(report.skipped_young, 1);
        assert!(fresh.exists());
        assert!(fresh.join("nested").join("payload.bin").exists());
    }

    #[test]
    fn federated_materialize_dirs_use_the_longer_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let materialize = make_dir_with_payload(tmp.path(), "cass-federated-materialize-r4nd0m");

        // Old enough for the lexical threshold but NOT the shared one.
        let report = reclaim_orphaned_staging_dirs_under_index_run_lock(
            tmp.path(),
            SystemTime::now() + LOCK_SERIALIZED_STAGING_MIN_AGE + Duration::from_secs(60),
        );
        assert_eq!(report.reclaimed_dirs, 0);
        assert_eq!(report.skipped_young, 1);
        assert!(materialize.exists());

        // Past the shared threshold it is reclaimed.
        let report = reclaim_orphaned_staging_dirs_under_index_run_lock(
            tmp.path(),
            future_now(SHARED_STAGING_MIN_AGE),
        );
        assert_eq!(report.reclaimed_dirs, 1, "errors: {:?}", report.errors);
        assert!(!materialize.exists());
    }

    #[test]
    fn non_staging_entries_are_never_touched() {
        let tmp = tempfile::tempdir().unwrap();
        let keepers = [
            "v6",
            "v8",
            ".lexical-publish-backups",
            "v7.bak.3",
            // Bare prefixes without the tempfile suffix are not names cass
            // ever creates; refuse them too.
            "cass-lexical-shards.",
            "cass-lexical-merge.",
            // Similar-but-different names.
            "cass-lexical-shardsX",
            "my-cass-lexical-merge.abc",
        ];
        for name in keepers {
            make_dir_with_payload(tmp.path(), name);
        }
        // A regular FILE with a matching name must also survive.
        fs::write(tmp.path().join("cass-lexical-merge.imafile"), b"data").unwrap();

        let report = reclaim_orphaned_staging_dirs_under_index_run_lock(
            tmp.path(),
            future_now(SHARED_STAGING_MIN_AGE),
        );

        assert_eq!(report.reclaimed_dirs, 0);
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        for name in keepers {
            assert!(tmp.path().join(name).exists(), "{name} must survive");
        }
        assert!(tmp.path().join("cass-lexical-merge.imafile").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_with_staging_names_are_skipped_and_targets_survive() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = make_dir_with_payload(tmp.path(), "precious-user-data");
        let link = tmp.path().join("cass-lexical-merge.sneaky");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        let report = reclaim_orphaned_staging_dirs_under_index_run_lock(
            tmp.path(),
            future_now(SHARED_STAGING_MIN_AGE),
        );

        assert_eq!(report.reclaimed_dirs, 0);
        assert!(link.exists(), "symlink itself must not be removed");
        assert!(victim.join("nested").join("payload.bin").exists());
    }

    #[test]
    fn interrupted_reclaim_markers_are_finished_on_the_next_sweep() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = make_dir_with_payload(tmp.path(), "cass-lexical-shards.abc.reclaim-42-0");

        let report = reclaim_orphaned_staging_dirs_under_index_run_lock(
            tmp.path(),
            future_now(LOCK_SERIALIZED_STAGING_MIN_AGE),
        );

        assert_eq!(report.reclaimed_dirs, 1, "errors: {:?}", report.errors);
        assert!(!marker.exists());
    }

    #[test]
    fn data_dir_wrapper_sweeps_both_index_root_and_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let index_root = tmp.path().join("index");
        fs::create_dir_all(&index_root).unwrap();
        let in_index = make_dir_with_payload(&index_root, "cass-lexical-merge.inindex");
        let top_level = make_dir_with_payload(tmp.path(), "cass-lexical-shards.toplevel");
        let live_index = make_dir_with_payload(&index_root, "v8");

        let report = reclaim_orphaned_staging_dirs_for_data_dir(
            tmp.path(),
            future_now(LOCK_SERIALIZED_STAGING_MIN_AGE),
        );

        assert_eq!(report.reclaimed_dirs, 2, "errors: {:?}", report.errors);
        assert!(!in_index.exists());
        assert!(!top_level.exists());
        assert!(live_index.exists(), "published index generations survive");
    }

    #[test]
    fn missing_staging_root_is_a_quiet_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let report = reclaim_orphaned_staging_dirs_under_index_run_lock(
            &tmp.path().join("does-not-exist"),
            future_now(SHARED_STAGING_MIN_AGE),
        );
        assert_eq!(report.reclaimed_dirs, 0);
        assert!(report.errors.is_empty());
    }
}
