//! Fixtureable source adapters for the planned `cass swarm status` surface.
//!
//! Live command adapters use bounded, read-only collection. Fixture adapters
//! retain the same snapshot contract for deterministic rendering tests.

use crate::pages::redact::{redact_swarm_json_value, redact_swarm_text};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Live collection never opens Beads' writable database. Its exported JSONL
/// snapshot is explicitly partial because unexported database changes may exist.
#[must_use]
pub fn collect_live_swarm_sources(repo: &Path) -> SwarmSourceCollection {
    let started = Instant::now();
    let snapshots = REQUIRED_SWARM_SOURCE_PROVIDERS
        .iter()
        .copied()
        .map(|name| {
            if matches!(
                name,
                SwarmProviderName::Git
                    | SwarmProviderName::Beads
                    | SwarmProviderName::Process
                    | SwarmProviderName::AgentMail
            ) {
                LiveSwarmSourceAdapter {
                    repo: repo.to_path_buf(),
                    name,
                    started,
                }
                .collect()
            } else {
                SwarmSourceSnapshot::unavailable(
                    name,
                    format!("live:{name}"),
                    "live-provider-unimplemented",
                    format!("live provider {name} is not wired yet"),
                )
            }
        })
        .collect();
    SwarmSourceCollection { snapshots }
}

struct LiveSwarmSourceAdapter {
    repo: PathBuf,
    name: SwarmProviderName,
    started: Instant,
}

impl SwarmSourceAdapter for LiveSwarmSourceAdapter {
    fn provider(&self) -> SwarmProviderName {
        self.name
    }

    fn collect(&self) -> SwarmSourceSnapshot {
        let started = Instant::now();
        let source = format!("live:{}", self.name);
        let result = match self.name {
            SwarmProviderName::Git => collect_live_git(&self.repo, self.started),
            SwarmProviderName::Beads => collect_exported_beads(&self.repo, self.started),
            SwarmProviderName::Process => collect_live_rch(&self.repo, self.started),
            SwarmProviderName::AgentMail => collect_live_mail(&self.repo, self.started),
            _ => Err("unsupported live provider".to_string()),
        };
        let mut snapshot = match result {
            Ok(payload) if self.name == SwarmProviderName::Beads => SwarmSourceSnapshot::partial(
                self.name,
                source,
                "Read-only exported JSONL snapshot; unexported Beads database changes are not observed. Recheck br before claiming work.",
                payload,
            ),
            Ok(payload) if self.name == SwarmProviderName::Process => SwarmSourceSnapshot::partial(
                self.name,
                source,
                "RCH status only; local processes and build admission are not observed.",
                payload,
            ),
            Ok(payload) if self.name == SwarmProviderName::AgentMail => {
                SwarmSourceSnapshot::partial(
                    self.name,
                    source,
                    "Read-only roster and reservation observations; messages and proof evidence are not collected. Recheck coordination before claiming.",
                    payload,
                )
            }
            Ok(payload) => SwarmSourceSnapshot::ok(self.name, source, payload),
            Err(error) => SwarmSourceSnapshot::unavailable(
                self.name,
                source,
                "live-provider-read-failed",
                error,
            ),
        };
        snapshot.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if self.name == SwarmProviderName::Beads {
            snapshot.freshness_ms = snapshot
                .payload
                .get("export_age_ms")
                .and_then(Value::as_u64);
        }
        snapshot
    }
}

fn live_command(
    repo: &Path,
    program: &str,
    args: &[&str],
    started: Instant,
) -> Result<Vec<u8>, String> {
    let remaining = Duration::from_secs(15)
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or("live provider deadline exceeded")?;
    let mut command = match program {
        "git" => Command::new("git"),
        "br" => Command::new("br"),
        "rch" => Command::new("rch"),
        #[cfg(test)]
        "sh" => Command::new("sh"),
        _ => return Err("unsupported live provider executable".to_string()),
    };
    command
        .args(args)
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("BEADS_DIR", repo.join(".beads"));
    // Caller-specific overrides must not redirect a project snapshot elsewhere.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "BEADS_DB",
        "BD_DB",
    ] {
        command.env_remove(key);
    }
    crate::sources::configure_child_process_group(&mut command);
    let child = command
        .spawn()
        .map_err(|error| format!("{program} spawn failed: {error}"))?;
    let output =
        crate::sources::wait_for_child_output_with_limit(child, remaining, Some(8 * 1024 * 1024))
            .map_err(|error| format!("{program} output failed: {error}"))?
            .ok_or_else(|| format!("{program} exceeded the live provider deadline"))?;
    if !output.status.success() {
        // Do not echo raw tool diagnostics, which may contain private paths/text.
        return Err(format!("{program} exited with {}", output.status));
    }
    Ok(output.stdout)
}

fn collect_live_mail(repo: &Path, started: Instant) -> Result<Value, String> {
    let endpoint = dotenvy::var("CASS_SWARM_AGENT_MAIL_URL")
        .map_err(|_| "CASS_SWARM_AGENT_MAIL_URL is not configured")?;
    let token = dotenvy::var("CASS_SWARM_AGENT_MAIL_TOKEN").ok();
    let project = repo
        .canonicalize()
        .map_err(|_| "cannot identify Mail project")?;
    let project = project.to_str().ok_or("Mail project is not UTF-8")?;
    let encoded: String = project.bytes().map(|byte| format!("%{byte:02X}")).collect();
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "cannot construct Mail HTTP client")?;
    let mail_started = Instant::now();
    let read = |uri: &str| -> Result<Value, String> {
        let remaining = Duration::from_secs(15)
            .checked_sub(started.elapsed())
            .ok_or("live provider deadline exceeded")?
            .min(
                Duration::from_secs(3)
                    .checked_sub(mail_started.elapsed())
                    .ok_or("Mail provider deadline exceeded")?,
            );
        if remaining.is_zero() {
            return Err("Mail provider deadline exceeded".into());
        }
        let mut request = client
            .post(&endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(remaining)
            .json(&serde_json::json!({
                "jsonrpc":"2.0", "id":"cass-swarm-read", "method":"resources/read",
                "params":{"uri":uri}
            }));
        if let Some(token) = &token {
            request = request.bearer_auth(token);
        }
        let response = request.send().map_err(|_| "Mail resource request failed")?;
        if !response.status().is_success() {
            return Err("Mail resource HTTP failure".into());
        }
        let mut bytes = Vec::new();
        response
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Mail resource body read failed")?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err("Mail response exceeds output cap".into());
        }
        parse_mail_resource(&bytes, uri)
    };
    let roster = read(&format!("resource://agents/{encoded}"))?;
    let reservations = read(&format!(
        "resource://file_reservations/{encoded}?active_only=true&limit=250"
    ))?;
    project_mail_observations(&roster, &reservations, project, chrono::Utc::now())
}

fn parse_mail_resource(bytes: &[u8], uri: &str) -> Result<Value, String> {
    let envelope: Value = serde_json::from_slice(bytes).map_err(|_| "invalid Mail JSON")?;
    if envelope["jsonrpc"] != "2.0"
        || envelope["id"] != "cass-swarm-read"
        || envelope.get("error").is_some()
    {
        return Err("invalid or rejected Mail response".into());
    }
    let contents = envelope["result"]["contents"]
        .as_array()
        .ok_or("missing Mail resource contents")?;
    if contents.len() != 1 || contents[0]["uri"] != uri {
        return Err("Mail resource identity mismatch".into());
    }
    serde_json::from_str(
        contents[0]["text"]
            .as_str()
            .ok_or("missing Mail resource text")?,
    )
    .map_err(|_| "invalid Mail resource JSON".into())
}

fn project_mail_observations(
    roster: &Value,
    reservations: &Value,
    project: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Value, String> {
    if roster["project"]["human_key"] != project {
        return Err("Mail project identity mismatch".into());
    }
    let agents = roster["agents"]
        .as_array()
        .ok_or("missing Mail agent roster")?;
    let reservations = reservations.as_array().ok_or("missing Mail reservations")?;
    // The reservation resource caps pages at 250 without a continuation flag.
    // A full page cannot prove a complete inventory.
    if agents.len() > 512 || reservations.len() >= 250 {
        return Err("Mail inventory exceeds bounded complete page".into());
    }
    let timestamp = |row: &Value,
                     field: &str|
     -> Result<chrono::DateTime<chrono::FixedOffset>, String> {
        chrono::DateTime::parse_from_rfc3339(row[field].as_str().ok_or("missing Mail timestamp")?)
            .map_err(|_| "invalid Mail timestamp".into())
    };
    let mut projected_agents = Vec::new();
    for agent in agents {
        let age = now.signed_duration_since(timestamp(agent, "last_active_ts")?);
        let name = agent["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or("missing Mail agent name")?;
        projected_agents.push(serde_json::json!({"name":name,
            "last_active_ts":agent["last_active_ts"],
            "active":age >= chrono::Duration::zero() && age <= chrono::Duration::seconds(600)}));
    }
    let mut projected_reservations = Vec::new();
    for reservation in reservations {
        let expires = timestamp(reservation, "expires_ts")?;
        let id = reservation["id"]
            .as_u64()
            .ok_or("invalid Mail reservation id")?;
        let agent = reservation["agent"]
            .as_str()
            .ok_or("missing Mail reservation agent")?;
        let path = reservation["path_pattern"]
            .as_str()
            .ok_or("missing Mail reservation path")?;
        let exclusive = reservation["exclusive"]
            .as_bool()
            .ok_or("missing Mail lock type")?;
        if reservation.get("released_ts").is_none() {
            return Err("missing Mail release state".into());
        }
        projected_reservations.push(
            serde_json::json!({"id":id,"agent":agent,"path_pattern":path,
            "exclusive":exclusive,"expires_ts":reservation["expires_ts"],
            "active":reservation["released_ts"].is_null() && expires > now}),
        );
    }
    let observed_active_reservation_count = projected_reservations
        .iter()
        .filter(|row| row["active"] == true)
        .count();
    Ok(
        serde_json::json!({"source_kind":"agent-mail-resources", "repository":project,
        "observed_at_ms":now.timestamp_millis(), "agents":projected_agents,
        "observed_active_reservation_count":observed_active_reservation_count,
        "reservations":projected_reservations}),
    )
}

fn collect_live_rch(repo: &Path, started: Instant) -> Result<Value, String> {
    let bytes = live_command(repo, "rch", &["status", "--json"], started)?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| "system clock precedes epoch")?
        .as_secs();
    parse_live_rch(&bytes, now)
}

fn parse_live_rch(bytes: &[u8], now_secs: u64) -> Result<Value, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| "invalid RCH JSON")?;
    if value["api_version"] != "1.0"
        || value["command"] != "status"
        || value["success"] != true
        || value["data"]["schema_version"] != "1.0.0"
    {
        return Err("unsupported or unsuccessful RCH status envelope".into());
    }
    let timestamp = value["timestamp"].as_u64().ok_or("missing RCH timestamp")?;
    if now_secs.saturating_sub(timestamp) > 60 || timestamp.saturating_sub(now_secs) > 5 {
        return Err("stale or future RCH status timestamp".into());
    }
    let data = &value["data"];
    let daemon = &data["daemon"];
    let active = daemon["active_builds"]
        .as_array()
        .ok_or("missing RCH active builds")?
        .len();
    let queued = daemon["queued_builds"]
        .as_array()
        .ok_or("missing RCH queued builds")?
        .len();
    let total = daemon["daemon"]["slots_total"]
        .as_u64()
        .ok_or("missing RCH total slots")?;
    let available = daemon["daemon"]["slots_available"]
        .as_u64()
        .ok_or("missing RCH available slots")?;
    if available > total {
        return Err("inconsistent RCH slot counts".into());
    }
    let posture = data["posture"].as_str().ok_or("missing RCH posture")?;
    if !matches!(posture, "remote_ready" | "degraded" | "local_only") {
        return Err("unknown RCH posture".into());
    }
    // Project an allowlist: worker addresses, commands and job details stay private.
    Ok(serde_json::json!({
        "source_kind": "rch-status", "schema_version": "1.0.0",
        "observed_at_ms": timestamp.checked_mul(1000).ok_or("RCH timestamp overflow")?,
        "active_rch_jobs": active, "queued_rch_jobs": queued,
        "slots_total": total, "slots_available": available, "fleet_posture": posture,
    }))
}

fn collect_live_git(repo: &Path, started: Instant) -> Result<Value, String> {
    let root = live_command(repo, "git", &["rev-parse", "--show-toplevel"], started)?;
    let root = std::str::from_utf8(&root).map_err(|_| "git root is not UTF-8")?;
    // Remove Git's line terminator, not whitespace that belongs to the path.
    let root = root.strip_suffix('\n').unwrap_or(root);
    let canonical = repo.canonicalize().map_err(|error| error.to_string())?;
    if Path::new(root)
        .canonicalize()
        .map_err(|error| error.to_string())?
        != canonical
    {
        return Err("run live swarm collection from the repository root".to_string());
    }
    let status = live_command(
        repo,
        "git",
        &[
            "-c",
            "core.fsmonitor=false",
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ],
        started,
    )?;
    let mut payload = parse_git_status(&status)?;
    payload["repository"] = Value::String(root.to_string());
    payload["repository_id"] = Value::String(
        blake3::hash(canonical.as_os_str().as_encoded_bytes())
            .to_hex()
            .to_string(),
    );
    payload["observed_at_ms"] = serde_json::json!(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    // Porcelain's branch.oid is the identity of the observed generation,
    // including null for an unborn branch. No fetch or index refresh occurs.
    Ok(payload)
}

fn parse_git_status(bytes: &[u8]) -> Result<Value, String> {
    if !bytes.ends_with(&[0]) {
        return Err("truncated git porcelain output".to_string());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "git porcelain paths are not UTF-8")?;
    let mut records = text.split_terminator('\0');
    let mut branch = None;
    let mut head = None;
    let mut saw_head = false;
    let mut upstream = None;
    let mut ahead = None;
    let mut behind = None;
    let mut paths = std::collections::BTreeSet::new();
    while let Some(record) = records.next() {
        if let Some(value) = record.strip_prefix("# branch.head ") {
            if value.is_empty() || branch.is_some() {
                return Err("invalid or duplicate git branch identity".to_string());
            }
            branch = Some(value.to_string());
        } else if let Some(value) = record.strip_prefix("# branch.oid ") {
            if saw_head
                || (value != "(initial)"
                    && (!matches!(value.len(), 40 | 64)
                        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())))
            {
                return Err("invalid or duplicate git commit identity".to_string());
            }
            saw_head = true;
            head = (value != "(initial)").then(|| value.to_string());
        } else if let Some(value) = record.strip_prefix("# branch.upstream ") {
            upstream = Some(value.to_string());
        } else if let Some(value) = record.strip_prefix("# branch.ab ") {
            let (a, b) = value
                .split_once(' ')
                .ok_or("invalid git ahead/behind counts")?;
            ahead = Some(
                a.strip_prefix('+')
                    .ok_or("invalid git ahead count")?
                    .parse::<u64>()
                    .map_err(|_| "invalid git ahead count")?,
            );
            behind = Some(
                b.strip_prefix('-')
                    .ok_or("invalid git behind count")?
                    .parse::<u64>()
                    .map_err(|_| "invalid git behind count")?,
            );
        } else if let Some(path) = record.strip_prefix("? ") {
            if path.is_empty() {
                return Err("empty git untracked path".to_string());
            }
            paths.insert(path.to_string());
        } else if record.starts_with("1 ") || record.starts_with("2 ") || record.starts_with("u ") {
            let fields = match record.as_bytes()[0] {
                b'1' => 9,
                b'2' => 10,
                _ => 11,
            };
            let path = record
                .splitn(fields, ' ')
                .nth(fields - 1)
                .filter(|path| !path.is_empty())
                .ok_or("invalid git path record")?;
            paths.insert(path.to_string());
            if record.starts_with("2 ") {
                let old = records
                    .next()
                    .filter(|path| !path.is_empty())
                    .ok_or("truncated git rename record")?;
                paths.insert(old.to_string());
            }
        } else {
            return Err("unsupported git porcelain record".to_string());
        }
    }
    let branch = branch.ok_or("missing git branch identity")?;
    if !saw_head {
        return Err("missing git commit identity".to_string());
    }
    // Use the same row contract as fixtures and the collision/evidence readers.
    // Bare strings would silently disappear when consumers inspect `path`.
    let dirty_paths: Vec<Value> = paths
        .iter()
        .map(|path| serde_json::json!({"path": path}))
        .collect();
    Ok(
        serde_json::json!({"branch": branch, "head": head, "upstream": upstream,
        "ahead": ahead, "behind": behind, "dirty": !paths.is_empty(), "dirty_paths": dirty_paths,
        "recent_commits": null}),
    )
}

fn collect_exported_beads(repo: &Path, started: Instant) -> Result<Value, String> {
    let path = repo.join(".beads/issues.jsonl");
    let before =
        fs::metadata(&path).map_err(|error| format!("Beads export unavailable: {error}"))?;
    let version = live_command(repo, "br", &["--version"], started)?;
    let version = supported_beads_version(&version)?;
    let read = |args: &[&str]| -> Result<Value, String> {
        let mut argv = vec!["--no-db", "--no-auto-import", "--no-auto-flush"];
        argv.extend_from_slice(args);
        let bytes = live_command(repo, "br", &argv, started)?;
        parse_beads_json(&bytes)
    };
    let ready = read(&["ready", "--json", "--limit", "513"])?;
    let active = read(&[
        "list",
        "--status",
        "in_progress",
        "--json",
        "--limit",
        "513",
    ])?;
    let blocked = read(&["blocked", "--json", "--limit", "513"])?;
    let active = beads_issue_page(&active)?;
    let blocked = beads_issue_page(&blocked)?;
    let project_rows = |rows: &Value| -> Result<Vec<Value>, String> {
        let rows = rows.as_array().ok_or("unsupported br issue schema")?;
        if rows.len() > 512 {
            return Err("Beads snapshot exceeds the 512-issue category limit".to_string());
        }
        rows.iter()
            .map(|row| {
                if row.get("id").and_then(Value::as_str).is_none()
                    || row.get("status").and_then(Value::as_str).is_none()
                {
                    return Err("Beads issue lacks identity or status".to_string());
                }
                let mut projected = serde_json::Map::new();
                for field in [
                    "id",
                    "title",
                    "status",
                    "priority",
                    "issue_type",
                    "assignee",
                    "labels",
                    "updated_at",
                    "blocked_by",
                ] {
                    if let Some(value) = row.get(field) {
                        projected.insert(field.to_string(), value.clone());
                    }
                }
                Ok(Value::Object(projected))
            })
            .collect()
    };
    let after = unchanged_beads_export(&path, &before)?;
    Ok(
        serde_json::json!({"ready": project_rows(&ready)?, "in_progress": project_rows(active)?,
        "blocked": project_rows(blocked)?,
        "graph": null, "version": version, "source_kind": "exported-jsonl",
        "observed_at_ms": SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis(),
        "export_age_ms": after.modified().ok().and_then(|time| time.elapsed().ok()).map(|age| age.as_millis())}),
    )
}

fn beads_issue_page(page: &Value) -> Result<&Value, String> {
    if page.get("has_more").and_then(Value::as_bool) != Some(false) {
        return Err("Beads page is truncated or has an unsupported schema".to_string());
    }
    page.get("issues")
        .filter(|issues| issues.is_array())
        .ok_or_else(|| "Beads page lacks an issues array".to_string())
}

fn unchanged_beads_export(path: &Path, before: &fs::Metadata) -> Result<fs::Metadata, String> {
    let after = fs::metadata(path).map_err(|error| error.to_string())?;
    if before.len() != after.len()
        || before.modified().map_err(|error| error.to_string())?
            != after.modified().map_err(|error| error.to_string())?
    {
        return Err("Beads export changed during collection; retry the snapshot".to_string());
    }
    Ok(after)
}

fn supported_beads_version(bytes: &[u8]) -> Result<&str, String> {
    let version = std::str::from_utf8(bytes)
        .map_err(|_| "invalid br version")?
        .trim();
    if version
        .strip_prefix("br 0.6.")
        .is_some_and(|patch| !patch.is_empty() && patch.bytes().all(|byte| byte.is_ascii_digit()))
    {
        Ok(version)
    } else {
        Err("unsupported br version; live reader supports 0.6.x".to_string())
    }
}

fn parse_beads_json(bytes: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(bytes).map_err(|_| "malformed or truncated br JSON".to_string())
}

/// Providers named by the swarm status contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmProviderName {
    AgentMail,
    Beads,
    CassHealth,
    CassStatus,
    DependencyDrift,
    Evidence,
    Git,
    Process,
    ResourcePlan,
    PrivacyExposure,
    ContextPack,
    WorkflowAnalytics,
    ReplayFixture,
    WorkflowMacros,
    ReproCapsule,
    OperationsDashboard,
}

impl SwarmProviderName {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentMail => "agent_mail",
            Self::Beads => "beads",
            Self::CassHealth => "cass_health",
            Self::CassStatus => "cass_status",
            Self::DependencyDrift => "dependency_drift",
            Self::Evidence => "evidence",
            Self::Git => "git",
            Self::Process => "process",
            Self::ResourcePlan => "resource_plan",
            Self::PrivacyExposure => "privacy_exposure",
            Self::ContextPack => "context_pack",
            Self::WorkflowAnalytics => "workflow_analytics",
            Self::ReplayFixture => "replay_fixture",
            Self::WorkflowMacros => "workflow_macros",
            Self::ReproCapsule => "repro_capsule",
            Self::OperationsDashboard => "operations_dashboard",
        }
    }

    #[must_use]
    pub const fn fixture_key(self) -> &'static str {
        match self {
            Self::Process => "processes",
            _ => self.as_str(),
        }
    }
}

impl fmt::Display for SwarmProviderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Required source providers from the current fixture contract.
pub const REQUIRED_SWARM_SOURCE_PROVIDERS: &[SwarmProviderName] = &[
    SwarmProviderName::AgentMail,
    SwarmProviderName::Beads,
    SwarmProviderName::CassHealth,
    SwarmProviderName::CassStatus,
    SwarmProviderName::Evidence,
    SwarmProviderName::Git,
    SwarmProviderName::Process,
];

/// Optional source providers available to richer status/evidence projections.
pub const OPTIONAL_SWARM_SOURCE_PROVIDERS: &[SwarmProviderName] = &[
    SwarmProviderName::DependencyDrift,
    SwarmProviderName::ResourcePlan,
    SwarmProviderName::PrivacyExposure,
    SwarmProviderName::ContextPack,
    SwarmProviderName::WorkflowAnalytics,
    SwarmProviderName::ReplayFixture,
    SwarmProviderName::WorkflowMacros,
    SwarmProviderName::ReproCapsule,
    SwarmProviderName::OperationsDashboard,
];

/// Every fixtureable provider named by the swarm status contract.
pub const ALL_SWARM_SOURCE_PROVIDERS: &[SwarmProviderName] = &[
    SwarmProviderName::AgentMail,
    SwarmProviderName::Beads,
    SwarmProviderName::CassHealth,
    SwarmProviderName::CassStatus,
    SwarmProviderName::DependencyDrift,
    SwarmProviderName::Evidence,
    SwarmProviderName::Git,
    SwarmProviderName::Process,
    SwarmProviderName::ResourcePlan,
    SwarmProviderName::PrivacyExposure,
    SwarmProviderName::ContextPack,
    SwarmProviderName::WorkflowAnalytics,
    SwarmProviderName::ReplayFixture,
    SwarmProviderName::WorkflowMacros,
    SwarmProviderName::ReproCapsule,
    SwarmProviderName::OperationsDashboard,
];

/// Provider availability normalized for robot output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SwarmProviderStatus {
    Ok,
    Partial,
    Unavailable,
    Skipped,
}

/// Where a diagnostic belongs. Provider stderr is kept out of stdout payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmDiagnosticStream {
    Stderr,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmProviderDiagnostic {
    pub stream: SwarmDiagnosticStream,
    pub message: String,
}

/// One provider snapshot, including typed status and raw provider payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwarmSourceSnapshot {
    pub name: SwarmProviderName,
    pub source: String,
    pub status: SwarmProviderStatus,
    pub freshness_ms: Option<u64>,
    pub elapsed_ms: u64,
    pub error_kind: Option<String>,
    pub warning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<SwarmProviderDiagnostic>,
    pub payload: Value,
}

impl SwarmSourceSnapshot {
    #[must_use]
    pub fn ok(name: SwarmProviderName, source: impl Into<String>, payload: Value) -> Self {
        Self {
            name,
            source: source.into(),
            status: SwarmProviderStatus::Ok,
            freshness_ms: Some(0),
            elapsed_ms: 0,
            error_kind: None,
            warning: None,
            diagnostics: Vec::new(),
            payload: redact_swarm_json_value(&payload),
        }
    }

    #[must_use]
    pub fn partial(
        name: SwarmProviderName,
        source: impl Into<String>,
        warning: impl Into<String>,
        payload: Value,
    ) -> Self {
        let warning = warning.into();
        let warning = redact_swarm_text(&warning);
        Self {
            name,
            source: source.into(),
            status: SwarmProviderStatus::Partial,
            freshness_ms: Some(0),
            elapsed_ms: 0,
            error_kind: None,
            warning: Some(warning.clone()),
            diagnostics: vec![SwarmProviderDiagnostic {
                stream: SwarmDiagnosticStream::Internal,
                message: warning,
            }],
            payload: redact_swarm_json_value(&payload),
        }
    }

    #[must_use]
    pub fn unavailable(
        name: SwarmProviderName,
        source: impl Into<String>,
        error_kind: impl Into<String>,
        warning: impl Into<String>,
    ) -> Self {
        let warning = warning.into();
        let warning = redact_swarm_text(&warning);
        Self {
            name,
            source: source.into(),
            status: SwarmProviderStatus::Unavailable,
            freshness_ms: None,
            elapsed_ms: 0,
            error_kind: Some(error_kind.into()),
            warning: Some(warning.clone()),
            diagnostics: vec![SwarmProviderDiagnostic {
                stream: SwarmDiagnosticStream::Stderr,
                message: warning,
            }],
            payload: Value::Null,
        }
    }

    #[must_use]
    pub fn skipped(
        name: SwarmProviderName,
        source: impl Into<String>,
        warning: impl Into<String>,
    ) -> Self {
        let warning = warning.into();
        let warning = redact_swarm_text(&warning);
        Self {
            name,
            source: source.into(),
            status: SwarmProviderStatus::Skipped,
            freshness_ms: None,
            elapsed_ms: 0,
            error_kind: None,
            warning: Some(warning.clone()),
            diagnostics: vec![SwarmProviderDiagnostic {
                stream: SwarmDiagnosticStream::Internal,
                message: warning,
            }],
            payload: Value::Null,
        }
    }
}

/// Common interface for live and fixture-backed swarm status providers.
pub trait SwarmSourceAdapter: Send + Sync {
    fn provider(&self) -> SwarmProviderName;
    fn collect(&self) -> SwarmSourceSnapshot;
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwarmSourceCollection {
    pub snapshots: Vec<SwarmSourceSnapshot>,
}

impl SwarmSourceCollection {
    #[must_use]
    pub fn partial(&self) -> bool {
        self.snapshots
            .iter()
            .any(|snapshot| snapshot.status != SwarmProviderStatus::Ok)
    }

    #[must_use]
    pub fn snapshot(&self, provider: SwarmProviderName) -> Option<&SwarmSourceSnapshot> {
        self.snapshots
            .iter()
            .find(|snapshot| snapshot.name == provider)
    }
}

#[must_use]
pub fn collect_swarm_sources<'a, I>(adapters: I) -> SwarmSourceCollection
where
    I: IntoIterator<Item = &'a dyn SwarmSourceAdapter>,
{
    SwarmSourceCollection {
        snapshots: adapters
            .into_iter()
            .map(SwarmSourceAdapter::collect)
            .collect(),
    }
}

#[derive(Debug, Clone)]
pub struct SwarmFixtureInput {
    path: PathBuf,
    fixture_id: String,
    description: Option<String>,
    sources: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct RawSwarmFixtureInput {
    fixture_id: String,
    #[serde(default)]
    description: Option<String>,
    sources: BTreeMap<String, Value>,
}

impl SwarmFixtureInput {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, SwarmSourceError> {
        let path = path.as_ref();
        let body = fs::read_to_string(path).map_err(|source| SwarmSourceError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let raw = serde_json::from_str::<RawSwarmFixtureInput>(&body).map_err(|source| {
            SwarmSourceError::Json {
                path: path.to_path_buf(),
                source,
            }
        })?;
        Self::from_raw(path.to_path_buf(), raw)
    }

    pub fn from_value(path: impl Into<PathBuf>, value: Value) -> Result<Self, SwarmSourceError> {
        let path = path.into();
        let raw = serde_json::from_value::<RawSwarmFixtureInput>(value).map_err(|source| {
            SwarmSourceError::Json {
                path: path.clone(),
                source,
            }
        })?;
        Self::from_raw(path, raw)
    }

    fn from_raw(path: PathBuf, raw: RawSwarmFixtureInput) -> Result<Self, SwarmSourceError> {
        if raw.fixture_id.trim().is_empty() {
            return Err(SwarmSourceError::InvalidFixture {
                path,
                reason: "fixture_id cannot be empty",
            });
        }
        Ok(Self {
            path,
            fixture_id: raw.fixture_id,
            description: raw.description,
            sources: raw.sources,
        })
    }

    #[must_use]
    pub fn fixture_id(&self) -> &str {
        &self.fixture_id
    }

    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn source_value(&self, provider: SwarmProviderName) -> Option<&Value> {
        self.sources.get(provider.fixture_key())
    }
}

#[derive(Debug, Clone)]
pub struct FixtureSwarmSourceAdapter {
    input: Arc<SwarmFixtureInput>,
    provider: SwarmProviderName,
}

impl FixtureSwarmSourceAdapter {
    #[must_use]
    pub fn new(input: Arc<SwarmFixtureInput>, provider: SwarmProviderName) -> Self {
        Self { input, provider }
    }
}

impl SwarmSourceAdapter for FixtureSwarmSourceAdapter {
    fn provider(&self) -> SwarmProviderName {
        self.provider
    }

    fn collect(&self) -> SwarmSourceSnapshot {
        let source = format!("fixture:{}", self.provider.fixture_key());
        match self.input.source_value(self.provider) {
            Some(value) => SwarmSourceSnapshot::ok(self.provider, source, value.clone()),
            None => SwarmSourceSnapshot::unavailable(
                self.provider,
                source,
                "missing-fixture-provider",
                format!(
                    "fixture {} at {} is missing provider source {}",
                    self.input.fixture_id(),
                    self.input.path().display(),
                    self.provider.fixture_key()
                ),
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FixtureSwarmAdapterSet {
    input: Arc<SwarmFixtureInput>,
}

impl FixtureSwarmAdapterSet {
    pub fn from_fixture_path(path: impl AsRef<Path>) -> Result<Self, SwarmSourceError> {
        Ok(Self {
            input: Arc::new(SwarmFixtureInput::from_path(path)?),
        })
    }

    #[must_use]
    pub fn from_input(input: SwarmFixtureInput) -> Self {
        Self {
            input: Arc::new(input),
        }
    }

    #[must_use]
    pub fn input(&self) -> &SwarmFixtureInput {
        &self.input
    }

    #[must_use]
    pub fn required_adapters(&self) -> Vec<FixtureSwarmSourceAdapter> {
        REQUIRED_SWARM_SOURCE_PROVIDERS
            .iter()
            .copied()
            .map(|provider| FixtureSwarmSourceAdapter::new(Arc::clone(&self.input), provider))
            .collect()
    }

    #[must_use]
    pub fn all_adapters(&self) -> Vec<FixtureSwarmSourceAdapter> {
        ALL_SWARM_SOURCE_PROVIDERS
            .iter()
            .copied()
            .map(|provider| FixtureSwarmSourceAdapter::new(Arc::clone(&self.input), provider))
            .collect()
    }

    #[must_use]
    pub fn collect_required(&self) -> SwarmSourceCollection {
        let adapters = self.required_adapters();
        collect_swarm_sources(
            adapters
                .iter()
                .map(|adapter| adapter as &dyn SwarmSourceAdapter),
        )
    }

    #[must_use]
    pub fn collect_all(&self) -> SwarmSourceCollection {
        let adapters = self.all_adapters();
        collect_swarm_sources(
            adapters
                .iter()
                .map(|adapter| adapter as &dyn SwarmSourceAdapter),
        )
    }
}

#[derive(Debug)]
pub enum SwarmSourceError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    InvalidFixture {
        path: PathBuf,
        reason: &'static str,
    },
}

impl fmt::Display for SwarmSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    f,
                    "failed to read swarm fixture {}: {source}",
                    path.display()
                )
            }
            Self::Json { path, source } => {
                write!(
                    f,
                    "failed to parse swarm fixture {}: {source}",
                    path.display()
                )
            }
            Self::InvalidFixture { path, reason } => {
                write!(f, "invalid swarm fixture {}: {reason}", path.display())
            }
        }
    }
}

impl Error for SwarmSourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json { source, .. } => Some(source),
            Self::InvalidFixture { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repo_path(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
    }

    #[test]
    fn mail_resource_rejects_wrong_identity_and_rpc_errors() {
        let uri = "resource://agents/%2Fproject";
        let response = json!({"jsonrpc":"2.0","id":"cass-swarm-read",
            "result":{"contents":[{"uri":uri,"text":"{\"agents\":[]}"}]}});
        assert_eq!(
            parse_mail_resource(&serde_json::to_vec(&response).unwrap(), uri).unwrap(),
            json!({"agents":[]})
        );
        assert!(parse_mail_resource(&serde_json::to_vec(&response).unwrap(), "other").is_err());
        for (pointer, replacement) in [
            ("/id", json!("other")),
            ("/jsonrpc", json!("1.0")),
            ("/result/contents/0/text", json!("bad JSON")),
            ("/result/contents", json!([])),
        ] {
            let mut bad = response.clone();
            *bad.pointer_mut(pointer).unwrap() = replacement;
            assert!(parse_mail_resource(&serde_json::to_vec(&bad).unwrap(), uri).is_err());
        }
        let mut failed = response;
        failed["error"] = json!({"message":"private failure"});
        assert!(parse_mail_resource(&serde_json::to_vec(&failed).unwrap(), uri).is_err());
    }

    #[test]
    fn mail_observations_use_live_time_and_omit_private_metadata() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-19T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let roster = json!({"project":{"human_key":"/project"},"agents":[
            {"name":"ActiveAgent","last_active_ts":"2026-09-18T23:50:00Z","task_description":"private task"},
            {"name":"OldAgent","last_active_ts":"2026-09-18T23:49:59Z"},
            {"name":"FutureAgent","last_active_ts":"2026-09-19T00:01:00Z"}
        ]});
        let reservation = json!({"id":1,"agent":"ActiveAgent","path_pattern":"src/**","exclusive":true,
            "expires_ts":"2026-09-19T00:00:01Z","released_ts":null,"reason":"private reason"});
        let mut expired = reservation.clone();
        expired["expires_ts"] = json!("2026-09-19T00:00:00Z");
        let mut released = reservation.clone();
        released["released_ts"] = json!("2026-09-18T23:59:00Z");
        let result = project_mail_observations(
            &roster,
            &json!([reservation, expired, released]),
            "/project",
            now,
        )
        .unwrap();
        assert_eq!(result["agents"][0]["active"], true);
        assert_eq!(result["agents"][1]["active"], false);
        assert_eq!(result["agents"][2]["active"], false);
        assert_eq!(result["reservations"][0]["active"], true);
        assert_eq!(result["reservations"][1]["active"], false);
        assert_eq!(result["reservations"][2]["active"], false);
        assert!(!result.to_string().contains("private"));
        assert!(project_mail_observations(&roster, &json!([]), "/wrong", now).is_err());
        assert!(
            project_mail_observations(&roster, &json!(vec![Value::Null; 250]), "/project", now)
                .is_err()
        );
        let mut bad = roster;
        bad["agents"][0]["last_active_ts"] = json!("invalid");
        assert!(project_mail_observations(&bad, &json!([]), "/project", now).is_err());
    }

    #[test]
    #[ignore = "requires explicitly configured live Mail HTTP endpoint; run through RCH"]
    fn live_mail_resource_read_preserves_metadata_only_contract() {
        let repo = std::env::current_dir().unwrap();
        let value = collect_live_mail(&repo, Instant::now()).expect("live Mail resources");
        assert_eq!(value["source_kind"], "agent-mail-resources");
        assert!(value["agents"].is_array());
        assert!(value["reservations"].is_array());
        assert!(value.get("messages").is_none());
        for agent in value["agents"].as_array().unwrap() {
            assert!(agent.get("task_description").is_none());
            assert!(agent["active"].is_boolean());
        }
    }

    fn rch_status_example() -> Value {
        json!({"api_version":"1.0", "command":"status", "success":true,
            "timestamp":1000, "data":{"schema_version":"1.0.0", "posture":"remote_ready",
                "daemon":{"daemon":{"slots_total":8,"slots_available":6,"socket_path":"private-socket"},
                    "active_builds":[{"command":"private-command"}], "queued_builds":[],
                    "workers":[{"host":"private-host"}]}}})
    }

    #[test]
    fn rch_status_projects_only_aggregate_observations() {
        let input = rch_status_example();
        let value = parse_live_rch(&serde_json::to_vec(&input).unwrap(), 1060).unwrap();
        assert_eq!(value["active_rch_jobs"], 1);
        assert_eq!(value["queued_rch_jobs"], 0);
        assert_eq!(value["slots_available"], 6);
        assert_eq!(value["observed_at_ms"], 1_000_000);
        assert!(!value.to_string().contains("private-"));
        assert!(value.get("active_cargo_jobs").is_none());
        assert!(value.get("recommended_action").is_none());
    }

    #[test]
    fn rch_status_rejects_stale_failed_and_incomplete_responses() {
        let input = rch_status_example();
        let bytes = serde_json::to_vec(&input).unwrap();
        assert!(parse_live_rch(&bytes, 1061).is_err());
        assert!(parse_live_rch(&bytes, 994).is_err());
        assert!(parse_live_rch(&bytes, 995).is_ok());
        assert!(parse_live_rch(b"not JSON", 1000).is_err());
        for (pointer, replacement) in [
            ("/api_version", json!("2.0")),
            ("/command", json!("status-fleet")),
            ("/success", json!(false)),
            ("/timestamp", Value::Null),
            ("/data/schema_version", json!("2.0.0")),
            ("/data/posture", json!("unknown")),
            ("/data/daemon/active_builds", Value::Null),
            ("/data/daemon/queued_builds", json!({})),
            ("/data/daemon/daemon/slots_total", json!(-1)),
            ("/data/daemon/daemon/slots_available", json!(9)),
        ] {
            let mut bad = input.clone();
            *bad.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                parse_live_rch(&serde_json::to_vec(&bad).unwrap(), 1000).is_err(),
                "{pointer}"
            );
        }
    }

    #[test]
    fn live_adapter_does_not_reset_the_request_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now().checked_sub(Duration::from_secs(16)).unwrap();
        for name in [SwarmProviderName::Git, SwarmProviderName::Process] {
            let snapshot = LiveSwarmSourceAdapter {
                repo: dir.path().into(),
                name,
                started,
            }
            .collect();
            assert_eq!(snapshot.status, SwarmProviderStatus::Unavailable);
            assert!(snapshot.payload.is_null());
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    #[ignore = "requires a live RCH daemon; run explicitly through RCH"]
    fn live_rch_status_is_partial_and_never_build_admission() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = LiveSwarmSourceAdapter {
            repo: dir.path().into(),
            name: SwarmProviderName::Process,
            started: Instant::now(),
        }
        .collect();
        assert_eq!(
            snapshot.status,
            SwarmProviderStatus::Partial,
            "{snapshot:?}"
        );
        assert!(snapshot.payload["active_rch_jobs"].is_u64());
        assert!(snapshot.payload.get("active_cargo_jobs").is_none());
        assert!(snapshot.payload.get("recommended_action").is_none());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn live_git_porcelain_preserves_rename_paths_and_unknown_upstream() {
        let bytes = b"# branch.oid 0123456789012345678901234567890123456789\0# branch.head main\x002 R. N... 100644 100644 100644 abc def R100 new name\0old\nname\0? untracked\0";
        let value = parse_git_status(bytes).expect("valid porcelain");
        assert_eq!(value["branch"], "main");
        assert_eq!(value["dirty"], true);
        assert_eq!(
            value["dirty_paths"],
            json!([{"path":"new name"}, {"path":"old\nname"}, {"path":"untracked"}])
        );
        assert!(value["ahead"].is_null());
        assert!(value["behind"].is_null());
        assert!(value["recent_commits"].is_null());
    }

    #[test]
    fn live_git_porcelain_rejects_truncated_and_unknown_records() {
        for bytes in [
            b"# branch.head main".as_slice(),
            b"# branch.head main\0".as_slice(),
            b"# branch.oid invalid\0# branch.head main\0".as_slice(),
            b"# branch.oid (initial)\0# branch.head \0".as_slice(),
            b"# branch.oid (initial)\0# branch.oid (initial)\0# branch.head main\0".as_slice(),
            b"# branch.oid (initial)\0# branch.head main\0# branch.head other\0".as_slice(),
            b"# branch.oid (initial)\0# branch.head main\0? \0".as_slice(),
            b"# branch.head main\0x future-record\0".as_slice(),
            b"# branch.head main\x002 R. N... 100644 100644 100644 a b R100 new\0".as_slice(),
            b"# branch.head main\0# branch.ab +wrong -0\0".as_slice(),
        ] {
            assert!(
                parse_git_status(bytes).is_err(),
                "accepted invalid porcelain: {bytes:?}"
            );
        }
        let clean = parse_git_status(b"# branch.oid (initial)\0# branch.head main\0")
            .expect("unborn branch");
        assert_eq!(clean["dirty"], false);
        assert!(clean["head"].is_null());
    }

    #[test]
    fn live_git_collection_reads_real_dirty_repository_without_writing_index() {
        let dir = tempfile::tempdir().expect("temporary repository");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git installed");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        let tracked = dir.path().join("tracked.txt");
        fs::write(&tracked, "first").expect("seed tracked file");
        git(&["add", "tracked.txt"]);
        git(&[
            "-c",
            "user.name=CASS Test",
            "-c",
            "user.email=cass@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "seed",
        ]);
        fs::write(&tracked, "changed").expect("dirty tracked file");
        fs::write(dir.path().join("new.txt"), "new").expect("untracked file");
        let index_path = dir.path().join(".git/index");
        let index_before = fs::read(&index_path).expect("read index");
        let modified_before = fs::metadata(&index_path)
            .expect("index metadata")
            .modified()
            .expect("mtime");
        let value = collect_live_git(dir.path(), Instant::now()).expect("live Git snapshot");
        assert_eq!(value["branch"], "main");
        assert_eq!(value["dirty"], true);
        assert_eq!(
            value["dirty_paths"],
            json!([{"path":"new.txt"}, {"path":"tracked.txt"}])
        );
        assert_eq!(fs::read(&tracked).expect("tracked bytes"), b"changed");
        assert_eq!(fs::read(&index_path).expect("index bytes"), index_before);
        assert_eq!(
            fs::metadata(&index_path)
                .expect("index metadata")
                .modified()
                .expect("mtime"),
            modified_before
        );
        assert!(!dir.path().join(".git/index.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn live_git_detects_dirty_submodules_and_preserves_trailing_path_spaces() {
        let sandbox = tempfile::tempdir().expect("temporary repositories");
        let parent = sandbox.path().join("parent with trailing space ");
        let upstream = sandbox.path().join("upstream");
        fs::create_dir(&parent).expect("parent directory");
        fs::create_dir(&upstream).expect("upstream directory");
        let git = |repo: &Path, args: &[&str]| {
            let output = Command::new("git")
                .args([
                    "-c",
                    "user.name=CASS Test",
                    "-c",
                    "user.email=cass@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .output()
                .expect("Git installed");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&upstream, &["init", "-b", "main"]);
        fs::write(upstream.join("tracked.txt"), "original").expect("submodule source");
        git(&upstream, &["add", "tracked.txt"]);
        git(&upstream, &["commit", "-m", "seed"]);
        git(&parent, &["init", "-b", "main"]);
        git(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                upstream.to_str().expect("temporary source path"),
                "module",
            ],
        );
        git(&parent, &["commit", "-m", "add submodule"]);
        // The explicit collector option must override a repository setting
        // that would otherwise conceal edits within this submodule.
        git(&parent, &["config", "submodule.module.ignore", "all"]);
        let clean = collect_live_git(&parent, Instant::now()).expect("clean parent");
        assert_eq!(clean["dirty"], false);
        fs::write(parent.join("module/tracked.txt"), "changed").expect("dirty submodule");
        let indexes = [
            parent.join(".git/index"),
            parent.join(".git/modules/module/index"),
        ];
        let before: Vec<_> = indexes
            .iter()
            .map(|path| {
                (
                    fs::read(path).unwrap(),
                    fs::metadata(path).unwrap().modified().unwrap(),
                )
            })
            .collect();
        let dirty = collect_live_git(&parent, Instant::now()).expect("dirty submodule snapshot");
        assert_eq!(dirty["dirty"], true);
        assert_eq!(dirty["dirty_paths"], json!([{"path":"module"}]));
        assert_eq!(
            dirty["repository"],
            parent.canonicalize().unwrap().to_str().unwrap()
        );
        for (path, (bytes, modified)) in indexes.iter().zip(before) {
            assert_eq!(fs::read(path).unwrap(), bytes);
            assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
        }
    }

    #[test]
    fn live_collection_reports_missing_sources_without_initializing_them() {
        let dir = tempfile::tempdir().expect("empty directory");
        let collection = collect_live_swarm_sources(dir.path());
        for name in [SwarmProviderName::Git, SwarmProviderName::Beads] {
            let snapshot = collection.snapshot(name).expect("provider retained");
            assert_eq!(snapshot.status, SwarmProviderStatus::Unavailable);
            assert!(snapshot.payload.is_null());
        }
        assert_eq!(
            fs::read_dir(dir.path()).expect("directory intact").count(),
            0
        );
    }

    #[test]
    fn live_command_refuses_an_expired_budget_before_spawning() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let started = Instant::now()
            .checked_sub(Duration::from_secs(16))
            .expect("earlier instant");
        let error = live_command(dir.path(), "nonexistent-cass-test-command", &[], started)
            .expect_err("deadline expired");
        assert!(error.contains("deadline"));
    }

    #[test]
    fn live_command_rejects_executables_outside_the_provider_allowlist() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let error = live_command(dir.path(), "echo", &["unexpected"], Instant::now())
            .expect_err("only provider executables are admitted");
        assert_eq!(error, "unsupported live provider executable");
    }

    #[cfg(unix)]
    #[test]
    fn live_command_terminates_a_child_that_outlives_the_budget() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let started = Instant::now()
            .checked_sub(Duration::from_secs(14))
            .expect("earlier instant");
        let error = live_command(dir.path(), "sh", &["-c", "exec sleep 10"], started)
            .expect_err("sleep must exceed the remaining one-second budget");
        assert!(error.contains("deadline"), "{error}");
    }

    #[test]
    fn live_beads_rejects_a_changed_export_and_preserves_its_age() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("issues.jsonl");
        fs::write(&path, "first export").expect("seed export");
        let old = SystemTime::now() - Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&path)
            .expect("open export")
            .set_times(fs::FileTimes::new().set_modified(old))
            .expect("old export timestamp");
        let before = fs::metadata(&path).expect("initial metadata");
        let unchanged = unchanged_beads_export(&path, &before).expect("stable old export");
        assert!(unchanged.modified().unwrap().elapsed().unwrap() >= Duration::from_secs(3599));
        fs::write(&path, "a changed export with a different length").expect("concurrent export");
        let error = unchanged_beads_export(&path, &before).expect_err("reject mixed snapshot");
        assert!(error.contains("changed during collection"));
    }

    #[test]
    fn live_beads_issue_pages_require_complete_pagination_metadata() {
        for issues in [json!([]), json!([{"id":"live-1", "status":"in_progress"}])] {
            let page = json!({"issues":issues, "has_more":false});
            assert_eq!(beads_issue_page(&page).unwrap(), &issues);
        }
        for page in [
            json!([]),
            json!({"issues":[]}),
            json!({"issues":[], "has_more":true}),
            json!({"issues":[], "has_more":"false"}),
            json!({"has_more":false}),
            json!({"issues":null, "has_more":false}),
            json!({"issues":{}, "has_more":false}),
        ] {
            assert!(beads_issue_page(&page).is_err(), "accepted page: {page}");
        }
    }

    #[test]
    fn live_beads_rejects_unknown_versions_and_invalid_machine_output() {
        assert_eq!(
            supported_beads_version(b"br 0.6.12\n").unwrap(),
            "br 0.6.12"
        );
        for bytes in [
            b"br 0.7.0".as_slice(),
            b"br 0.6.".as_slice(),
            b"br 0.6.1-dev".as_slice(),
            b"br 0.6.future".as_slice(),
            b"br \xff".as_slice(),
        ] {
            assert!(supported_beads_version(bytes).is_err());
        }
        assert_eq!(parse_beads_json(b"[]\n").unwrap(), json!([]));
        for bytes in [
            b"[{\"id\":\"partial".as_slice(),
            b"diagnostic\n[]".as_slice(),
            b"[] trailing output".as_slice(),
            b"\xff".as_slice(),
        ] {
            assert!(parse_beads_json(bytes).is_err());
        }
    }

    #[test]
    #[ignore = "requires installed br 0.6.x; run explicitly through RCH"]
    fn live_beads_collection_reads_real_tracker_without_writing_it() {
        let dir = tempfile::tempdir().expect("temporary tracker");
        let br = |args: &[&str]| {
            let output = Command::new("br")
                .args(args)
                .current_dir(dir.path())
                .env("BEADS_DIR", dir.path().join(".beads"))
                .env_remove("BEADS_DB")
                .env_remove("BD_DB")
                .output()
                .expect("br 0.6.x installed");
            assert!(
                output.status.success(),
                "br {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        br(&["init", "--prefix", "live"]);
        let create = |title: &str| {
            let bytes = br(&["create", title, "--json"]);
            let value: Value = serde_json::from_slice(&bytes).expect("created issue");
            value["id"].as_str().expect("created ID").to_string()
        };
        let ready = create("Ready task");
        let active = create("Active task");
        let blocked = create("Blocked task");
        br(&["update", &active, "--status", "in_progress"]);
        br(&["dep", "add", &blocked, &active]);
        br(&["sync", "--flush-only"]);
        let before: Vec<_> = ["issues.jsonl", "beads.db"]
            .into_iter()
            .map(|name| {
                let path = dir.path().join(".beads").join(name);
                let bytes = fs::read(&path).expect("tracker file");
                let modified = fs::metadata(&path)
                    .expect("tracker metadata")
                    .modified()
                    .expect("mtime");
                (path, bytes, modified)
            })
            .collect();
        let snapshot = LiveSwarmSourceAdapter {
            repo: dir.path().to_path_buf(),
            name: SwarmProviderName::Beads,
            started: Instant::now(),
        }
        .collect();
        assert_eq!(
            snapshot.status,
            SwarmProviderStatus::Partial,
            "{snapshot:?}"
        );
        assert_eq!(snapshot.payload["ready"][0]["id"], ready);
        assert_eq!(snapshot.payload["in_progress"][0]["id"], active);
        assert_eq!(snapshot.payload["blocked"][0]["id"], blocked);
        assert!(snapshot.payload["graph"].is_null());
        for (path, bytes, modified) in before {
            assert_eq!(fs::read(&path).expect("tracker intact"), bytes);
            assert_eq!(
                fs::metadata(&path)
                    .expect("metadata intact")
                    .modified()
                    .expect("mtime"),
                modified
            );
        }
    }

    #[test]
    fn fixture_adapter_collects_every_required_provider_from_healthy_fixture() {
        let adapters = FixtureSwarmAdapterSet::from_fixture_path(repo_path(
            "tests/fixtures/swarm_status/healthy.inputs.json",
        ))
        .expect("healthy fixture should parse");

        let collection = adapters.collect_required();

        assert!(!collection.partial());
        assert_eq!(
            collection
                .snapshots
                .iter()
                .map(|snapshot| snapshot.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "agent_mail",
                "beads",
                "cass_health",
                "cass_status",
                "evidence",
                "git",
                "process"
            ]
        );
        assert_eq!(
            collection
                .snapshot(SwarmProviderName::Beads)
                .and_then(|snapshot| snapshot.payload["ready"].as_array())
                .map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn missing_fixture_provider_becomes_unavailable_snapshot() {
        let input = SwarmFixtureInput::from_value(
            "inline-missing.json",
            json!({
                "fixture_id": "missing-provider",
                "sources": {
                    "beads": {"ready": []}
                }
            }),
        )
        .expect("inline fixture should parse");
        let set = FixtureSwarmAdapterSet::from_input(input);

        let collection = set.collect_required();
        let missing = collection
            .snapshot(SwarmProviderName::AgentMail)
            .expect("agent_mail snapshot should exist");

        assert!(collection.partial());
        assert_eq!(missing.status, SwarmProviderStatus::Unavailable);
        assert_eq!(
            missing.error_kind.as_deref(),
            Some("missing-fixture-provider")
        );
        assert_eq!(missing.payload, Value::Null);
        assert_eq!(
            missing
                .diagnostics
                .first()
                .map(|diagnostic| diagnostic.stream),
            Some(SwarmDiagnosticStream::Stderr)
        );
    }

    #[test]
    fn process_provider_uses_contract_name_and_fixture_key() {
        let input = SwarmFixtureInput::from_value(
            "inline-process.json",
            json!({
                "fixture_id": "process-provider",
                "sources": {
                    "processes": {"active_rch_jobs": 2}
                }
            }),
        )
        .expect("inline fixture should parse");
        let adapter = FixtureSwarmSourceAdapter::new(Arc::new(input), SwarmProviderName::Process);
        let snapshot = adapter.collect();

        assert_eq!(SwarmProviderName::Process.as_str(), "process");
        assert_eq!(SwarmProviderName::Process.fixture_key(), "processes");
        assert_eq!(snapshot.name, SwarmProviderName::Process);
        assert_eq!(snapshot.source, "fixture:processes");
        assert_eq!(snapshot.status, SwarmProviderStatus::Ok);
        assert_eq!(snapshot.payload["active_rch_jobs"], 2);
    }

    #[test]
    fn status_variants_serialize_to_contract_values() {
        assert_eq!(
            serde_json::to_string(&SwarmProviderStatus::Ok).unwrap(),
            r#""ok""#
        );
        assert_eq!(
            serde_json::to_string(&SwarmProviderStatus::Partial).unwrap(),
            r#""partial""#
        );
        assert_eq!(
            serde_json::to_string(&SwarmProviderStatus::Unavailable).unwrap(),
            r#""unavailable""#
        );
        assert_eq!(
            serde_json::to_string(&SwarmProviderStatus::Skipped).unwrap(),
            r#""skipped""#
        );
    }

    #[test]
    fn partial_and_skipped_snapshots_are_degraded_and_redacted() {
        let partial = SwarmSourceSnapshot::partial(
            SwarmProviderName::Git,
            "fixture:git",
            "partial fixture read at /home/alice/private-client with TOKEN=SECRET_VALUE",
            json!({
                "path": "/home/alice/private-client/src/lib.rs",
                "dirty_by_path": {
                    "/home/alice/private-client/src/lib.rs": "modified"
                },
                "command": "env TOKEN=SECRET_VALUE cargo test",
                "evidence_ref": "pack:///data/projects/private-client/session.jsonl#L44"
            }),
        );
        let skipped = SwarmSourceSnapshot::skipped(
            SwarmProviderName::Evidence,
            "fixture:evidence",
            "skipped optional evidence probe for /home/alice/private-client",
        );
        let collection = SwarmSourceCollection {
            snapshots: vec![partial, skipped],
        };

        assert!(collection.partial());
        let git = collection
            .snapshot(SwarmProviderName::Git)
            .expect("git snapshot should exist");
        let evidence = collection
            .snapshot(SwarmProviderName::Evidence)
            .expect("evidence snapshot should exist");

        assert_eq!(git.status, SwarmProviderStatus::Partial);
        assert_eq!(evidence.status, SwarmProviderStatus::Skipped);
        assert_eq!(git.diagnostics[0].stream, SwarmDiagnosticStream::Internal);
        assert_eq!(
            evidence.diagnostics[0].stream,
            SwarmDiagnosticStream::Internal
        );
        assert_eq!(git.payload["evidence_ref"], "pack://[REDACTED_PATH]#L44");
        assert!(
            git.payload["dirty_by_path"]
                .as_object()
                .is_some_and(|paths| paths.contains_key("[REDACTED_PATH]"))
        );

        let serialized =
            serde_json::to_string(&collection.snapshots).expect("snapshots should serialize");
        assert!(!serialized.contains("/home/alice"));
        assert!(!serialized.contains("/data/projects/private-client"));
        assert!(!serialized.contains("SECRET_VALUE"));
        assert!(serialized.contains("[REDACTED_PATH]"));
        assert!(serialized.contains("[SECRET_ENV_REDACTED]"));
    }

    #[test]
    fn required_adapters_collects_evidence_provider() {
        let input = SwarmFixtureInput::from_value(
            "inline-evidence.json",
            json!({
                "fixture_id": "evidence-provider",
                "sources": {
                    "agent_mail": {"messages": []},
                    "beads": {"ready": []},
                    "cass_health": {"healthy": true},
                    "cass_status": {"search_ready": true},
                    "git": {"dirty": false},
                    "processes": {"active_rch_jobs": 0},
                    "evidence": {
                        "recent_proofs": [
                            {
                                "ref": "pack:///data/projects/private-client/session.jsonl#L44",
                                "status": "redacted"
                            }
                        ]
                    }
                }
            }),
        )
        .expect("inline fixture should parse");
        let set = FixtureSwarmAdapterSet::from_input(input);

        let collection = set.collect_required();
        let evidence = collection
            .snapshot(SwarmProviderName::Evidence)
            .expect("evidence snapshot should exist");

        assert_eq!(
            collection.snapshots.len(),
            REQUIRED_SWARM_SOURCE_PROVIDERS.len()
        );
        assert!(!collection.partial());
        assert_eq!(evidence.status, SwarmProviderStatus::Ok);
        assert_eq!(evidence.source, "fixture:evidence");
        assert_eq!(
            evidence.payload["recent_proofs"][0]["ref"],
            "pack://[REDACTED_PATH]#L44"
        );
    }

    #[test]
    fn fixture_payload_strings_pass_through_redaction_layer() {
        let input = SwarmFixtureInput::from_value(
            "inline-redaction.json",
            json!({
                "fixture_id": "redaction-provider",
                "sources": {
                    "git": {
                        "dirty_paths": [
                            {"path": "/home/alice/private-client/src/lib.rs"}
                        ],
                        "dirty_by_path": {
                            "/home/alice/private-client/src/lib.rs": "modified"
                        },
                        "last_author": "alice@example.com",
                        "command": "env TOKEN=SECRET_VALUE CARGO_TARGET_DIR=/home/alice/cass-target cargo test",
                        "evidence_ref": "pack:///data/projects/private-client/session.jsonl#L44"
                    }
                }
            }),
        )
        .expect("inline fixture should parse");
        let adapter = FixtureSwarmSourceAdapter::new(Arc::new(input), SwarmProviderName::Git);
        let snapshot = adapter.collect();
        let serialized = serde_json::to_string(&snapshot.payload).expect("payload serializes");

        assert!(!serialized.contains("/home/alice"));
        assert!(!serialized.contains("/data/projects/private-client"));
        assert!(!serialized.contains("alice@example.com"));
        assert!(!serialized.contains("SECRET_VALUE"));
        assert_eq!(
            snapshot.payload["evidence_ref"],
            "pack://[REDACTED_PATH]#L44"
        );
        assert!(
            snapshot.payload["dirty_by_path"]
                .as_object()
                .is_some_and(|paths| paths.contains_key("[REDACTED_PATH]"))
        );
        assert!(serialized.contains("[REDACTED_PATH]"));
        assert!(serialized.contains("[EMAIL_REDACTED]"));
        assert!(serialized.contains("[SECRET_ENV_REDACTED]"));
    }

    #[test]
    fn collector_consumes_only_the_adapter_trait() {
        let input = Arc::new(
            SwarmFixtureInput::from_value(
                "inline-trait.json",
                json!({
                    "fixture_id": "trait-collector",
                    "sources": {
                        "beads": {"ready": []},
                        "git": {"dirty": false}
                    }
                }),
            )
            .expect("inline fixture should parse"),
        );
        let adapters = [
            FixtureSwarmSourceAdapter::new(Arc::clone(&input), SwarmProviderName::Beads),
            FixtureSwarmSourceAdapter::new(Arc::clone(&input), SwarmProviderName::Git),
        ];
        let trait_refs = adapters
            .iter()
            .map(|adapter| adapter as &dyn SwarmSourceAdapter);

        let collection = collect_swarm_sources(trait_refs);

        assert_eq!(collection.snapshots.len(), 2);
        assert_eq!(
            collection.snapshot(SwarmProviderName::Git).unwrap().status,
            SwarmProviderStatus::Ok
        );
    }

    #[test]
    fn checked_in_swarm_fixtures_provide_all_required_sources() {
        for name in [
            "healthy",
            "busy",
            "stale_advisory",
            "reservation_conflict",
            "build_pressure",
            "no_ready_work",
            "privacy_guardrails",
        ] {
            let path = repo_path(&format!("tests/fixtures/swarm_status/{name}.inputs.json"));
            let adapters = FixtureSwarmAdapterSet::from_fixture_path(path)
                .unwrap_or_else(|err| panic!("{name} fixture should parse: {err}"));
            let collection = adapters.collect_required();

            assert!(
                !collection.partial(),
                "{name} fixture should provide every required provider: {collection:#?}"
            );
        }
    }
}
