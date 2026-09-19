//! Metadata-only repository intake for the live operations dashboard.
//!
//! Git child output is discarded, not buffered. The Beads reader retains only
//! bounded lifecycle metadata, never descriptions, comments, or session text.
//! This is an advisory local task view, not Agent Mail reservation authority.

use std::collections::BTreeMap;
use std::fs::{self, Metadata, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::pages::redact::redact_swarm_text;
use crate::swarm_status::{SwarmProviderName, SwarmSourceSnapshot};

const MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_RECORDS: usize = 50_000;
const MAX_VISIBLE_TASKS: usize = 20;
const READ_BUDGET: Duration = Duration::from_secs(3);
const GIT_BUDGET: Duration = Duration::from_secs(2);

fn unavailable(name: SwarmProviderName, kind: &'static str) -> SwarmSourceSnapshot {
    SwarmSourceSnapshot::unavailable(name, format!("live:{}", name.as_str()), kind, kind)
}

/// Resolve a worktree boundary without invoking a shell or reading gitdir
/// targets. A linked worktree's regular `.git` file is a valid marker. Without
/// Git, the nearest `.beads` directory is an explicitly local project boundary.
fn repository_root(start: &Path) -> Result<PathBuf, &'static str> {
    let start = start
        .canonicalize()
        .map_err(|_| "repository-root-unreadable")?;
    if !start.is_dir() {
        return Err("repository-root-not-directory");
    }
    let mut beads_root = None;
    for (depth, candidate) in start.ancestors().enumerate() {
        if depth >= 64 {
            return Err("repository-root-depth-limit");
        }
        match fs::symlink_metadata(candidate.join(".git")) {
            Ok(meta) if meta.is_dir() || meta.is_file() => return Ok(candidate.to_path_buf()),
            Ok(_) => return Err("repository-marker-not-regular"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("repository-marker-unreadable"),
        }
        if beads_root.is_none()
            && fs::symlink_metadata(candidate.join(".beads")).is_ok_and(|meta| meta.is_dir())
        {
            beads_root = Some(candidate.to_path_buf());
        }
    }
    Ok(beads_root.unwrap_or(start))
}

pub(super) fn collect(start: &Path) -> Value {
    let (git, beads) = match repository_root(start) {
        Ok(root) => (collect_git(&root), collect_beads(&root)),
        Err(kind) => (
            unavailable(SwarmProviderName::Git, kind),
            unavailable(SwarmProviderName::Beads, kind),
        ),
    };
    json!({
        "git": git,
        "beads": beads,
        "coordination_verified": false,
        "readiness_basis": "open, unassigned, non-epic tasks whose direct blocks dependencies are closed in this complete local snapshot; other blocking semantics require review",
        "unavailable_coordination": ["agent_mail_reservations", "agent_mail_metadata", "rch_jobs"],
        "collection_contract": {
            "read_only": true, "network": false, "session_content_read": false,
            "git_child_output_retained": false,
            "beads_byte_limit": MAX_BYTES, "beads_record_limit": MAX_RECORDS,
            "visible_tasks_per_list": MAX_VISIBLE_TASKS,
            "git_deadline_ms": GIT_BUDGET.as_millis(),
            "beads_cooperative_budget_ms": READ_BUDGET.as_millis(),
            "filesystem_calls_have_hard_deadline": false
        }
    })
}

/// Only index/ref comparisons: no worktree content, clean filters, untracked
/// traversal, submodule recursion, or external diff/textconv helpers.
fn collect_git(root: &Path) -> SwarmSourceSnapshot {
    let started = Instant::now();
    let deadline = started + GIT_BUDGET;
    let probes = [
        &[
            "diff",
            "--cached",
            "--quiet",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
            "--",
        ][..],
        &["symbolic-ref", "--quiet", "HEAD"][..],
        &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"][..],
    ];
    let mut states = Vec::new();
    for args in probes {
        match git_exit(root, args, deadline) {
            Ok(code) => states.push(code),
            Err(kind) => {
                let mut result = unavailable(SwarmProviderName::Git, kind);
                result.elapsed_ms = elapsed_ms(started);
                return result;
            }
        }
    }
    let mut result = SwarmSourceSnapshot::partial(
        SwarmProviderName::Git,
        "live:git",
        "Only staged changes and HEAD/merge state were inspected; unstaged worktree and untracked files were not scanned.",
        json!({
            "staged_changes": states[0] == 1,
            "detached_head": states[1] == 1,
            "merge_in_progress": states[2] == 0,
            "unstaged_changes": null, "untracked_files": null,
            "submodules_inspected": false,
            "observations_atomic": false
        }),
    );
    result.elapsed_ms = elapsed_ms(started);
    result
}

fn git_exit(root: &Path, args: &[&str], deadline: Instant) -> Result<i32, &'static str> {
    if Instant::now() >= deadline {
        return Err("git-deadline-exceeded");
    }
    let mut command = Command::new("git");
    // Keep PATH/system loader settings, but remove inherited Git routing,
    // injected config, tracing destinations and redirected stdio handles.
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    command
        .args(["--no-pager", "--no-optional-locks", "-C"])
        .arg(root)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "core.hooksPath=",
            "-c",
            "diff.external=",
            "-c",
            "protocol.allow=never",
        ])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        // Empty allowlist overrides per-protocol configuration, including
        // partial-clone lazy fetch on Git versions predating NO_LAZY_FETCH.
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TRACE2", "0")
        .env("GIT_TRACE2_EVENT", "0")
        .env("GIT_TRACE2_PERF", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| "git-spawn-failed")?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return match status.code() {
                    Some(code @ (0 | 1)) => Ok(code),
                    _ => Err("git-probe-failed"),
                };
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            outcome => {
                // Always reap the direct child; no detached reader threads or
                // inherited output pipes can hold this collector open.
                let killed = child.kill();
                let reaped = child.wait();
                if killed.is_err() || reaped.is_err() {
                    return Err("git-child-cleanup-failed");
                }
                return Err(if outcome.is_err() {
                    "git-wait-failed"
                } else {
                    "git-deadline-exceeded"
                });
            }
        }
    }
}

#[derive(Deserialize)]
struct Bead {
    id: String,
    status: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    issue_type: String,
    #[serde(default)]
    priority: Option<u8>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    depends_on_id: String,
    #[serde(rename = "type")]
    kind: String,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
}

fn bounded_text(text: &str, chars: usize) -> String {
    // Redact before truncating, so cutting a credential cannot defeat the
    // redactor's full-token pattern. Control characters never reach the TUI.
    redact_swarm_text(text)
        .chars()
        .filter(|c| !c.is_control())
        .take(chars)
        .collect()
}

fn collect_beads(root: &Path) -> SwarmSourceSnapshot {
    let started = Instant::now();
    let outcome = read_beads(root, started + READ_BUDGET);
    let mut snapshot = match outcome {
        Ok((records, rejected, freshness)) => {
            let payload = project_beads(&records, rejected);
            let mut snapshot = if rejected == 0 {
                SwarmSourceSnapshot::ok(SwarmProviderName::Beads, "live:beads", payload)
            } else {
                SwarmSourceSnapshot::partial(
                    SwarmProviderName::Beads,
                    "live:beads",
                    "Some records were rejected; counts are lower bounds and runnable candidates are suppressed.",
                    payload,
                )
            };
            snapshot.freshness_ms = freshness;
            snapshot
        }
        Err(kind) => unavailable(SwarmProviderName::Beads, kind),
    };
    snapshot.elapsed_ms = elapsed_ms(started);
    snapshot
}

fn same_metadata(left: &Metadata, right: &Metadata) -> bool {
    if left.len() != right.len() || left.modified().ok() != right.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (left.dev(), left.ino(), left.ctime(), left.ctime_nsec())
            != (right.dev(), right.ino(), right.ctime(), right.ctime_nsec())
        {
            return false;
        }
    }
    true
}

/// Beads keyed by id, the number of records scanned, and the newest mtime seen.
type ReadBeadsOutcome = (BTreeMap<String, Bead>, usize, Option<u64>);

fn read_beads(root: &Path, deadline: Instant) -> Result<ReadBeadsOutcome, &'static str> {
    let directory = root.join(".beads");
    let parent = fs::symlink_metadata(&directory).map_err(|_| "beads-directory-unavailable")?;
    if !parent.is_dir() {
        return Err("beads-directory-not-regular");
    }
    let path = directory.join("issues.jsonl");
    let before = fs::symlink_metadata(&path).map_err(|_| "beads-snapshot-unavailable")?;
    if !before.is_file() {
        return Err("beads-snapshot-not-regular");
    }
    if before.len() > MAX_BYTES {
        return Err("beads-byte-limit");
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(&path)
        .map_err(|_| "beads-snapshot-open-failed")?;
    let opened = file.metadata().map_err(|_| "beads-snapshot-stat-failed")?;
    if !opened.is_file() || !same_metadata(&before, &opened) {
        return Err("beads-snapshot-changed");
    }
    let result = parse_beads(&file, deadline)?;
    let after = file.metadata().map_err(|_| "beads-snapshot-stat-failed")?;
    let path_after = fs::symlink_metadata(&path).map_err(|_| "beads-snapshot-changed")?;
    let parent_after = fs::symlink_metadata(&directory).map_err(|_| "beads-snapshot-changed")?;
    if !path_after.is_file()
        || !parent_after.is_dir()
        || !same_metadata(&before, &after)
        || !same_metadata(&before, &path_after)
        || !same_metadata(&parent, &parent_after)
    {
        return Err("beads-snapshot-changed");
    }
    let freshness = before
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64);
    Ok((result.0, result.1, freshness))
}

fn parse_beads(
    reader: impl Read,
    deadline: Instant,
) -> Result<(BTreeMap<String, Bead>, usize), &'static str> {
    let mut reader = BufReader::new(reader.take(MAX_BYTES + 1));
    let mut records = BTreeMap::new();
    let mut rejected = 0usize;
    let mut bytes = 0u64;
    let mut count = 0usize;
    let mut line = Vec::new();
    loop {
        if Instant::now() >= deadline {
            return Err("beads-read-budget-exceeded");
        }
        line.clear();
        let read = reader
            .by_ref()
            .take((MAX_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|_| "beads-read-failed")?;
        if read == 0 {
            break;
        }
        bytes += read as u64;
        if bytes > MAX_BYTES {
            return Err("beads-byte-limit");
        }
        if line.len() > MAX_LINE_BYTES {
            return Err("beads-record-byte-limit");
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        count += 1;
        if count > MAX_RECORDS {
            return Err("beads-record-count-limit");
        }
        let Ok(mut bead) = serde_json::from_slice::<Bead>(&line) else {
            rejected += 1;
            continue;
        };
        if !valid_id(&bead.id)
            || bead.priority.is_some_and(|priority| priority > 4)
            || bead.issue_type.len() > 64
            || bead.dependencies.len() > 1024
            || bead
                .dependencies
                .iter()
                .any(|dependency| !valid_id(&dependency.depends_on_id))
            || !matches!(
                bead.status.as_str(),
                "open" | "in_progress" | "blocked" | "deferred" | "closed" | "tombstone"
            )
        {
            rejected += 1;
            continue;
        }
        bead.title = bounded_text(&bead.title, 240);
        bead.assignee = bead
            .assignee
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| bounded_text(value, 160));
        if records.insert(bead.id.clone(), bead).is_some() {
            // Last-write-wins would make stale/duplicated snapshots authorize
            // unsafe work. Refuse rather than guessing which record is current.
            return Err("beads-duplicate-identity");
        }
    }
    Ok((records, rejected))
}

fn project_beads(records: &BTreeMap<String, Bead>, rejected: usize) -> Value {
    let mut counts = BTreeMap::<&str, usize>::new();
    let mut tasks = Vec::new();
    let mut runnable_count = 0usize;
    let mut blocked_count = 0usize;
    let mut review_count = 0usize;
    for bead in records.values() {
        *counts.entry(&bead.status).or_default() += 1;
        if !matches!(bead.status.as_str(), "open" | "in_progress" | "blocked") {
            continue;
        }
        let mut blockers = Vec::new();
        let mut unknown = false;
        for dependency in &bead.dependencies {
            match dependency.kind.as_str() {
                "blocks" => match records.get(&dependency.depends_on_id) {
                    Some(predecessor) if predecessor.status == "closed" => {}
                    Some(_) => blockers.push(dependency.depends_on_id.clone()),
                    None => {
                        unknown = true;
                        blockers.push(dependency.depends_on_id.clone());
                    }
                },
                "related" | "parent-child" | "discovered-from" => {}
                _ => unknown = true,
            }
        }
        blockers.sort();
        blockers.dedup();
        let readiness = if rejected != 0 || unknown {
            review_count += 1;
            "review-required"
        } else if bead.status == "blocked" || !blockers.is_empty() {
            blocked_count += 1;
            "blocked"
        } else if bead.status == "in_progress" || bead.assignee.is_some() {
            "assigned"
        } else if bead.issue_type == "epic" {
            "coordination-only"
        } else {
            runnable_count += 1;
            "locally-unblocked"
        };
        tasks.push((bead.priority.unwrap_or(4), bead.id.as_str(), json!({
            "id": bounded_text(&bead.id, 160), "title": bead.title,
            "priority": bead.priority, "status": bead.status,
            "assignee": bead.assignee, "readiness": readiness,
            "blocker_count": blockers.len(),
            "blockers": blockers.iter().take(20).map(|id| bounded_text(id, 160)).collect::<Vec<_>>(),
            "blockers_truncated": blockers.len() > 20
        })));
    }
    tasks.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)));
    let ready = tasks
        .iter()
        .filter(|(_, _, task)| task["readiness"] == "locally-unblocked")
        .take(MAX_VISIBLE_TASKS)
        .map(|(_, _, task)| task.clone())
        .collect::<Vec<_>>();
    let active = tasks
        .iter()
        .filter(|(_, _, task)| task["status"] == "in_progress" || !task["assignee"].is_null())
        .take(MAX_VISIBLE_TASKS)
        .map(|(_, _, task)| task.clone())
        .collect::<Vec<_>>();
    let attention = tasks
        .iter()
        .filter(|(_, _, task)| {
            matches!(
                task["readiness"].as_str(),
                Some("blocked" | "review-required")
            )
        })
        .take(MAX_VISIBLE_TASKS)
        .map(|(_, _, task)| task.clone())
        .collect::<Vec<_>>();
    json!({
        "complete": rejected == 0, "accepted_records": records.len(), "rejected_records": rejected,
        "counts": counts, "locally_unblocked_count": runnable_count,
        "dependency_blocked_count": blocked_count, "review_required_count": review_count,
        "candidate_tasks": ready, "active_tasks": active, "attention_tasks": attention,
        "task_lists_bounded": true, "coordination_verified": false
    })
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

/// Closed, bounded projection, also used for fixtures. A caller cannot smuggle
/// descriptions, raw paths, commands or arbitrary provider fields into HTML.
pub(super) fn project(source: Option<&Value>) -> Option<Value> {
    let source = source?.as_object()?;
    let git = source.get("git");
    let beads = source.get("beads");
    let gp = git.and_then(|value| value.get("payload"));
    let bp = beads.and_then(|value| value.get("payload"));
    let complete = bp
        .and_then(|value| value.get("complete"))
        .and_then(Value::as_bool)
        == Some(true)
        && beads
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            == Some("ok");
    let task_list = |key: &str| -> Vec<Value> {
        bp.and_then(|value| value.get(key)).and_then(Value::as_array).into_iter().flatten()
            .take(MAX_VISIBLE_TASKS).map(|task| json!({
                "id": display_field(Some(task), "id", 160),
                "title": display_field(Some(task), "title", 240),
                "priority": task.get("priority").and_then(Value::as_u64).filter(|value| *value <= 4),
                "status": display_field(Some(task), "status", 32),
                "readiness": display_field(Some(task), "readiness", 40),
                "assignee": display_field(Some(task), "assignee", 160),
                "blocker_count": task.get("blocker_count").and_then(Value::as_u64),
                "blockers": task.get("blockers").and_then(Value::as_array).into_iter().flatten()
                    .filter_map(Value::as_str).take(20).map(|value| bounded_text(value, 160)).collect::<Vec<_>>()
            })).collect()
    };
    let candidates = if complete {
        task_list("candidate_tasks")
    } else {
        Vec::new()
    };
    let mut counts = serde_json::Map::new();
    for status in [
        "open",
        "in_progress",
        "blocked",
        "deferred",
        "closed",
        "tombstone",
    ] {
        let count = bp
            .and_then(|value| value.get("counts"))
            .and_then(|value| value.get(status))
            .and_then(Value::as_u64);
        counts.insert(
            status.into(),
            if complete {
                json!(count.unwrap_or(0))
            } else {
                json!(count)
            },
        );
    }
    let staged = gp
        .and_then(|value| value.get("staged_changes"))
        .and_then(Value::as_bool);
    let merge = gp
        .and_then(|value| value.get("merge_in_progress"))
        .and_then(Value::as_bool);
    let blocked = bp
        .and_then(|value| value.get("dependency_blocked_count"))
        .and_then(Value::as_u64);
    let review = bp
        .and_then(|value| value.get("review_required_count"))
        .and_then(Value::as_u64);
    let status = if merge == Some(true)
        || blocked.is_some_and(|count| count > 0)
        || review.is_some_and(|count| count > 0)
    {
        "warning"
    } else {
        "partial"
    }; // Reservation and worktree coverage are always missing.
    Some(json!({
        "status": status,
        "git": {
            "provider": provider_fields(git), "staged_changes": staged,
            "detached_head": gp.and_then(|value| value.get("detached_head")).and_then(Value::as_bool),
            "merge_in_progress": merge, "unstaged_changes": null, "untracked_files": null
        },
        "beads": {
            "provider": provider_fields(beads), "complete": complete, "counts": counts,
            "locally_unblocked_count": if complete { bp.and_then(|value| value.get("locally_unblocked_count")).and_then(Value::as_u64) } else { None },
            "dependency_blocked_count": blocked, "review_required_count": review,
            "candidate_tasks": candidates, "active_tasks": task_list("active_tasks"),
            "attention_tasks": task_list("attention_tasks")
        },
        "coordination_verified": false,
        "readiness_basis": "Local dependency metadata only; confirm Agent Mail reservations before starting work.",
        "limitations": ["worktree and untracked files not inspected", "Agent Mail reservations not inspected", "RCH jobs not inspected"],
        "inspect_command": "cass swarm dashboard --json"
    }))
}

fn display_field(source: Option<&Value>, field: &str, max: usize) -> Option<String> {
    source
        .and_then(|value| value.get(field))
        .and_then(Value::as_str)
        .map(|value| bounded_text(value, max))
}

fn provider_fields(source: Option<&Value>) -> Value {
    let status = source
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str);
    json!({
        "status": status.filter(|value| matches!(*value, "ok" | "partial" | "unavailable" | "skipped")).unwrap_or("unavailable"),
        "error_kind": display_field(source, "error_kind", 80),
        "warning": display_field(source, "warning", 320),
        "freshness_ms": source.and_then(|value| value.get("freshness_ms")).and_then(Value::as_u64),
        "elapsed_ms": source.and_then(|value| value.get("elapsed_ms")).and_then(Value::as_u64)
    })
}

pub(super) fn html(card: &Value) -> String {
    let field = |pointer: &str| {
        card.pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or("unavailable")
    };
    let boolean = |pointer: &str| match card.pointer(pointer).and_then(Value::as_bool) {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    };
    let mut body = format!(
        "<p><strong>Git:</strong> {}. Staged changes: {}; merge in progress: {}.</p><p><strong>Beads:</strong> {}. Complete local snapshot: {}.</p>",
        super::html_escape(field("/git/provider/status")),
        boolean("/git/staged_changes"),
        boolean("/git/merge_in_progress"),
        super::html_escape(field("/beads/provider/status")),
        boolean("/beads/complete")
    );
    for (key, title) in [
        ("candidate_tasks", "Locally unblocked candidates"),
        ("active_tasks", "Assigned or active tasks"),
        ("attention_tasks", "Blocked or review required"),
    ] {
        body.push_str(&format!("<h3>{title}</h3><ul>"));
        let tasks = card
            .pointer(&format!("/beads/{key}"))
            .and_then(Value::as_array);
        if tasks.is_none_or(Vec::is_empty) {
            body.push_str("<li class=\"muted\">None reported by this snapshot.</li>");
        }
        for task in tasks.into_iter().flatten().take(MAX_VISIBLE_TASKS) {
            let text = |key| {
                super::html_escape(task.get(key).and_then(Value::as_str).unwrap_or("unknown"))
            };
            body.push_str(&format!(
                "<li><code>{}</code> — {}<br><span class=\"muted\">{}</span></li>",
                text("id"),
                text("title"),
                text("readiness")
            ));
        }
        body.push_str("</ul>");
    }
    body.push_str("<p class=\"muted\">Local metadata only. Confirm reservations before starting work. Unstaged files, untracked files, Agent Mail and RCH were not inspected.</p>");
    format!("<section class=\"card\"><h2>Repository and task cockpit</h2>{body}</section>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> (BTreeMap<String, Bead>, usize) {
        parse_beads(body.as_bytes(), Instant::now() + Duration::from_secs(30)).unwrap()
    }
    fn payload(body: &str) -> Value {
        let (records, rejected) = parse(body);
        project_beads(&records, rejected)
    }
    fn bead(id: &str, status: &str, priority: u8) -> Value {
        json!({"id": id, "title": id, "status": status, "issue_type": "task", "priority": priority})
    }
    fn lines(records: &[Value]) -> String {
        records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn candidates_require_closed_predecessors_and_are_sorted_by_priority_then_id() {
        let done = bead("done", "closed", 0);
        let mut blocked = bead("blocked", "open", 0);
        blocked["dependencies"] = json!([{"depends_on_id": "open", "type": "blocks"}]);
        let mut ready = bead("ready", "open", 1);
        ready["dependencies"] = json!([{"depends_on_id": "done", "type": "blocks"}]);
        let p = payload(&lines(&[
            done,
            blocked,
            ready,
            bead("open", "open", 2),
            bead("alpha", "open", 1),
        ]));
        assert_eq!(
            p["candidate_tasks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|task| task["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["alpha", "ready", "open"]
        );
        assert_eq!(p["dependency_blocked_count"], 1);
    }

    #[test]
    fn unknown_dependency_semantics_and_missing_predecessors_require_review() {
        for dependency in [
            json!({"depends_on_id": "missing", "type": "blocks"}),
            json!({"depends_on_id": "done", "type": "conditional-blocks"}),
        ] {
            let mut task = bead("task", "open", 0);
            task["dependencies"] = json!([dependency]);
            let p = payload(&lines(&[task, bead("done", "closed", 0)]));
            assert_eq!(p["locally_unblocked_count"], 0);
            assert_eq!(p["review_required_count"], 1);
        }
    }

    #[test]
    fn assigned_in_progress_and_epics_are_never_start_candidates() {
        let mut assigned = bead("assigned", "open", 0);
        assigned["assignee"] = json!("BlueAgent");
        let mut epic = bead("epic", "open", 0);
        epic["issue_type"] = json!("epic");
        let p = payload(&lines(&[assigned, epic, bead("busy", "in_progress", 0)]));
        assert_eq!(p["locally_unblocked_count"], 0);
        assert_eq!(p["active_tasks"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn malformed_or_unknown_records_suppress_all_runnable_claims() {
        for invalid in [
            "{not-json}",
            r#"{"id":"bad","status":"unknown"}"#,
            r#"{"id":"bad","status":"open","priority":9}"#,
        ] {
            let p = payload(&format!("{}\n{invalid}\n", bead("valid", "open", 0)));
            assert_eq!(p["complete"], false);
            assert_eq!(p["rejected_records"], 1);
            assert_eq!(p["locally_unblocked_count"], 0);
        }
    }

    #[test]
    fn duplicate_ids_are_rejected_instead_of_last_write_wins() {
        let input = lines(&[bead("same", "closed", 0), bead("same", "open", 0)]);
        assert!(matches!(
            parse_beads(input.as_bytes(), Instant::now() + READ_BUDGET),
            Err("beads-duplicate-identity")
        ));
    }

    #[test]
    fn record_byte_and_elapsed_budgets_fail_without_partial_ready_output() {
        let oversized = vec![b' '; MAX_LINE_BYTES + 1];
        assert!(matches!(
            parse_beads(oversized.as_slice(), Instant::now() + READ_BUDGET),
            Err("beads-record-byte-limit")
        ));
        assert!(matches!(
            parse_beads(&b""[..], Instant::now()),
            Err("beads-read-budget-exceeded")
        ));
    }

    #[test]
    fn visible_task_lists_are_bounded_but_total_counts_are_not_truncated() {
        let input = lines(
            &(0..57)
                .map(|id| bead(&format!("task-{id:03}"), "open", 1))
                .collect::<Vec<_>>(),
        );
        let p = payload(&input);
        assert_eq!(p["locally_unblocked_count"], 57);
        assert_eq!(
            p["candidate_tasks"].as_array().unwrap().len(),
            MAX_VISIBLE_TASKS
        );
    }

    #[test]
    fn projection_rejects_arbitrary_content_and_does_not_infer_missing_counts() {
        let card = project(Some(
            &json!({"beads": {"status": "unavailable", "payload": {
            "complete": true, "candidate_tasks": [{"id": "forged", "title": "not eligible"}],
            "description": "private body"
        }}, "unknown": "private body"}),
        ))
        .unwrap();
        assert!(card["beads"]["counts"]["open"].is_null());
        assert!(
            card["beads"]["candidate_tasks"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(!card.to_string().contains("private body"));
        assert_eq!(card["coordination_verified"], false);
    }

    #[test]
    fn nested_repository_and_beads_only_roots_are_resolved_without_writes() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        fs::create_dir(temp.path().join(".beads"))?;
        let nested = temp.path().join("one/two");
        fs::create_dir_all(&nested)?;
        assert_eq!(
            repository_root(&nested).unwrap(),
            temp.path().canonicalize()?
        );
        fs::write(
            temp.path().join(".git"),
            "gitdir: /unused-worktree-metadata\n",
        )?;
        assert_eq!(
            repository_root(&nested).unwrap(),
            temp.path().canonicalize()?
        );
        Ok(())
    }

    #[test]
    fn file_reader_preserves_bytes_and_reports_missing_sources() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        assert_eq!(
            collect_beads(temp.path()).status,
            crate::swarm_status::SwarmProviderStatus::Unavailable
        );
        let directory = temp.path().join(".beads");
        fs::create_dir(&directory)?;
        let path = directory.join("issues.jsonl");
        let body = lines(&[bead("ready", "open", 0)]);
        fs::write(&path, &body)?;
        let before = fs::metadata(&path)?.modified()?;
        let result = collect_beads(temp.path());
        assert_eq!(result.status, crate::swarm_status::SwarmProviderStatus::Ok);
        assert_eq!(result.payload["locally_unblocked_count"], 1);
        assert_eq!(fs::read_to_string(&path)?, body);
        assert_eq!(fs::metadata(&path)?.modified()?, before);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn final_and_parent_symlinks_are_not_read() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let private = temp.path().join("private");
        fs::create_dir(&private)?;
        fs::write(
            private.join("issues.jsonl"),
            lines(&[bead("secret", "open", 0)]),
        )?;
        let root = temp.path().join("root");
        fs::create_dir(&root)?;
        std::os::unix::fs::symlink(&private, root.join(".beads"))?;
        assert_eq!(
            collect_beads(&root).error_kind.as_deref(),
            Some("beads-directory-not-regular")
        );
        let other = temp.path().join("other");
        fs::create_dir_all(other.join(".beads"))?;
        std::os::unix::fs::symlink(
            private.join("issues.jsonl"),
            other.join(".beads/issues.jsonl"),
        )?;
        assert_eq!(
            collect_beads(&other).error_kind.as_deref(),
            Some("beads-snapshot-not-regular")
        );
        Ok(())
    }

    #[test]
    fn untrusted_task_text_is_redacted_before_html_escaping() {
        let mut task = bead("safe-id", "open", 1);
        task["title"] = json!("inspect /home/alice/private <script>alert(1)</script>");
        task["description"] = json!("DO-NOT-PUBLISH-THIS-BODY");
        let p = payload(&lines(&[task]));
        let card = project(Some(&json!({"beads": {"status": "ok", "payload": p}}))).unwrap();
        let html = html(&card);
        for text in [card.to_string(), html.clone()] {
            assert!(!text.contains("DO-NOT-PUBLISH-THIS-BODY"));
            assert!(!text.contains("/home/alice/private"));
        }
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;") || html.contains("REDACTED"));
    }
}
