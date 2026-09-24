//! `cass schedule` — keep the index fresh with the operating system's own
//! scheduler instead of a resident process.
//!
//! Two jobs are registered:
//!
//! * **incremental** (default every 15 min): `cass index --background`
//!   (incremental; cheap: readdir+stat, unchanged files are never re-parsed),
//!   preceded by any remote-source syncs whose `sync_schedule` in
//!   `sources.toml` is due. This is the executor `sync_schedule = "hourly" |
//!   "daily"` never had.
//! * **nightly** (default 03:00 local): remote syncs that are due, a full
//!   `cass index --full --background`, then bounded semantic backfill batches
//!   (`cass models backfill --scheduled --max-batches N`) in one worker per
//!   tier — first the fast (hash) tier, then
//!   the quality (MiniLM) tier when the model is installed — until the
//!   backlog drains or the scheduler gates (load, console idle) say stop.
//!
//! Priority is delegated to the OS: launchd uses `Nice=15` without
//! background I/O throttling, while systemd uses `Nice=19` +
//! `IOSchedulingClass=idle` + `CPUSchedulingPolicy=idle`. Each job step is a
//! child `cass` process, so the normal `index-run.lock` / exit-7 `index-busy`
//! contract protects against a human running `cass index` at the same moment.
//!
//! Everything the job does is recorded under `<data_dir>/schedule/`:
//! `state.json` (last run per job), `runs.jsonl` (append-only history), and
//! per-job log files the OS scheduler writes stdout/stderr into.
//!
//! Known honest failure: on a machine with **zero** agent sessions, the
//! nightly `index --full` fails cass's own post-publish validation (there is
//! no lexical index to read back), so `schedule status` shows a failed
//! nightly until the first session exists. Incremental runs succeed on an
//! empty corpus.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::indexer::background_refresh::background_index_args;
use crate::indexer::responsiveness;

mod discovery;
mod execution;
mod outcomes;

pub use discovery::{SourceScheduleError, due_remote_sources};

pub const DEFAULT_INTERVAL_MINS: u32 = 15;
pub const DEFAULT_NIGHTLY_HOUR: u8 = 3;
pub const DEFAULT_NIGHTLY_MINUTE: u8 = 0;
pub const DEFAULT_MAX_BACKFILL_BATCHES: u32 = 200;
pub const LAUNCHD_LABEL_PREFIX: &str = "com.dicklesworthstone.cass";
pub const SYSTEMD_UNIT_PREFIX: &str = "cass";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ScheduleJob {
    /// Incremental index (+ due remote syncs). Cheap; runs every N minutes.
    Incremental,
    /// Full index + semantic backfill (+ due remote syncs). Runs once a day.
    Nightly,
}

impl ScheduleJob {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Incremental => "incremental",
            Self::Nightly => "nightly",
        }
    }

    pub fn launchd_label(self) -> String {
        format!("{LAUNCHD_LABEL_PREFIX}.{}", self.as_str())
    }

    pub fn systemd_unit_base(self) -> String {
        format!("{SYSTEMD_UNIT_PREFIX}-{}", self.as_str())
    }
}

/// What the operator asked `schedule install` to register.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ScheduleSpec {
    pub interval_mins: u32,
    pub nightly_hour: u8,
    pub nightly_minute: u8,
    pub nightly: bool,
    pub semantic: bool,
    /// Absolute cass binary the units invoke.
    pub binary: PathBuf,
    /// Explicit data dir baked into the units so a scheduler environment
    /// without `CASS_DATA_DIR` still targets the right archive.
    pub data_dir: PathBuf,
    pub db_path: Option<PathBuf>,
}

impl ScheduleSpec {
    pub fn interval_secs(&self) -> u32 {
        self.interval_mins.max(1).saturating_mul(60)
    }

    /// argv (after the binary) for one job.
    pub fn job_args(&self, job: ScheduleJob) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(db) = &self.db_path {
            args.push("--db".to_string());
            args.push(db.display().to_string());
        }
        args.push("--color=never".to_string());
        args.push("schedule".to_string());
        args.push("run".to_string());
        args.push("--job".to_string());
        args.push(job.as_str().to_string());
        args.push("--data-dir".to_string());
        args.push(self.data_dir.display().to_string());
        args.push("--json".to_string());
        if !self.semantic {
            args.push("--no-semantic".to_string());
        }
        args
    }

    pub fn jobs(&self) -> Vec<ScheduleJob> {
        if self.nightly {
            vec![ScheduleJob::Incremental, ScheduleJob::Nightly]
        } else {
            vec![ScheduleJob::Incremental]
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Launchd,
    Systemd,
    Unsupported,
}

impl Platform {
    pub fn detect() -> Self {
        if cfg!(target_os = "macos") {
            Self::Launchd
        } else if cfg!(target_os = "linux") {
            Self::Systemd
        } else {
            Self::Unsupported
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Launchd => "launchd",
            Self::Systemd => "systemd",
            Self::Unsupported => "unsupported",
        }
    }
}

/// One file the installer writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UnitFile {
    pub path: PathBuf,
    pub contents: String,
}

pub fn schedule_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("schedule")
}

pub fn state_path(data_dir: &Path) -> PathBuf {
    schedule_dir(data_dir).join("state.json")
}

pub fn runs_log_path(data_dir: &Path) -> PathBuf {
    schedule_dir(data_dir).join("runs.jsonl")
}

pub fn job_log_path(data_dir: &Path, job: ScheduleJob) -> PathBuf {
    schedule_dir(data_dir).join(format!("{}.log", job.as_str()))
}

/// Where units live for the current user.
pub fn unit_dir(
    platform: Platform,
    home: &Path,
    xdg_config_home: Option<&Path>,
) -> Option<PathBuf> {
    match platform {
        Platform::Launchd => Some(home.join("Library").join("LaunchAgents")),
        Platform::Systemd => Some(
            xdg_config_home
                .map(Path::to_path_buf)
                .unwrap_or_else(|| home.join(".config"))
                .join("systemd")
                .join("user"),
        ),
        Platform::Unsupported => None,
    }
}

fn xml_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render the launchd property list for one job.
pub fn render_launchd_plist(spec: &ScheduleSpec, job: ScheduleJob) -> String {
    let mut argv = vec![spec.binary.display().to_string()];
    argv.extend(spec.job_args(job));
    let args_xml: String = argv
        .iter()
        .map(|a| format!("        <string>{}</string>\n", xml_escape(a)))
        .collect();
    let trigger = match job {
        ScheduleJob::Incremental => format!(
            "    <key>StartInterval</key>\n    <integer>{}</integer>\n",
            spec.interval_secs()
        ),
        ScheduleJob::Nightly => format!(
            "    <key>StartCalendarInterval</key>\n    <dict>\n        <key>Hour</key>\n        <integer>{}</integer>\n        <key>Minute</key>\n        <integer>{}</integer>\n    </dict>\n",
            spec.nightly_hour, spec.nightly_minute
        ),
    };
    let log = job_log_path(&spec.data_dir, job).display().to_string();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
    <key>Label</key>\n\
    <string>{label}</string>\n\
    <key>ProgramArguments</key>\n\
    <array>\n{args_xml}    </array>\n\
{trigger}\
    <key>RunAtLoad</key>\n\
    <false/>\n\
    <key>Nice</key>\n\
    <integer>15</integer>\n\
    <key>StandardOutPath</key>\n\
    <string>{log}</string>\n\
    <key>StandardErrorPath</key>\n\
    <string>{log}</string>\n\
    <key>EnvironmentVariables</key>\n\
    <dict>\n\
        <key>CASS_INDEX_NO_PROGRESS_EVENTS</key>\n\
        <string>1</string>\n\
    </dict>\n\
</dict>\n\
</plist>\n",
        label = job.launchd_label(),
        log = xml_escape(&log),
    )
}

fn systemd_quote(raw: &str) -> String {
    if raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_' | '=' | ':' | ','))
    {
        raw.to_string()
    } else {
        format!("\"{}\"", raw.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// Render the systemd user service for one job.
pub fn render_systemd_service(spec: &ScheduleSpec, job: ScheduleJob) -> String {
    let mut argv = vec![systemd_quote(&spec.binary.display().to_string())];
    argv.extend(spec.job_args(job).iter().map(|a| systemd_quote(a)));
    let description = match job {
        ScheduleJob::Incremental => "cass incremental index refresh (background priority)",
        ScheduleJob::Nightly => "cass nightly full index + semantic backfill (background priority)",
    };
    let log = job_log_path(&spec.data_dir, job).display().to_string();
    format!(
        "[Unit]\n\
Description={description}\n\
Documentation=https://github.com/Dicklesworthstone/coding_agent_session_search\n\
\n\
[Service]\n\
Type=oneshot\n\
ExecStart={exec}\n\
Nice=19\n\
IOSchedulingClass=idle\n\
CPUSchedulingPolicy=idle\n\
Environment=CASS_INDEX_NO_PROGRESS_EVENTS=1\n\
StandardOutput=append:{log}\n\
StandardError=append:{log}\n\
TimeoutStartSec={timeout}\n",
        exec = argv.join(" "),
        log = log,
        timeout = match job {
            ScheduleJob::Incremental => "30min",
            ScheduleJob::Nightly => "6h",
        },
    )
}

/// Render the systemd user timer for one job.
pub fn render_systemd_timer(spec: &ScheduleSpec, job: ScheduleJob) -> String {
    let (description, trigger) = match job {
        ScheduleJob::Incremental => (
            "cass incremental index timer",
            format!(
                "OnBootSec=5min\nOnUnitActiveSec={}min\n",
                spec.interval_mins.max(1)
            ),
        ),
        ScheduleJob::Nightly => (
            "cass nightly index timer",
            format!(
                "OnCalendar=*-*-* {:02}:{:02}:00\n",
                spec.nightly_hour, spec.nightly_minute
            ),
        ),
    };
    format!(
        "[Unit]\n\
Description={description}\n\
\n\
[Timer]\n\
{trigger}\
Persistent=true\n\
RandomizedDelaySec=60\n\
Unit={unit}.service\n\
\n\
[Install]\n\
WantedBy=timers.target\n",
        unit = job.systemd_unit_base(),
    )
}

/// All unit files for a spec on a platform.
pub fn render_units(spec: &ScheduleSpec, platform: Platform, unit_dir: &Path) -> Vec<UnitFile> {
    let mut out = Vec::new();
    for job in spec.jobs() {
        match platform {
            Platform::Launchd => out.push(UnitFile {
                path: unit_dir.join(format!("{}.plist", job.launchd_label())),
                contents: render_launchd_plist(spec, job),
            }),
            Platform::Systemd => {
                let base = job.systemd_unit_base();
                out.push(UnitFile {
                    path: unit_dir.join(format!("{base}.service")),
                    contents: render_systemd_service(spec, job),
                });
                out.push(UnitFile {
                    path: unit_dir.join(format!("{base}.timer")),
                    contents: render_systemd_timer(spec, job),
                });
            }
            Platform::Unsupported => {}
        }
    }
    out
}

/// A shell command the installer ran (or would run under `--dry-run`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommandRecord {
    pub argv: Vec<String>,
    pub executed: bool,
    pub exit_code: Option<i32>,
    pub stderr: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstallReport {
    pub platform: Platform,
    pub dry_run: bool,
    pub unit_dir: Option<PathBuf>,
    pub units: Vec<UnitFile>,
    pub commands: Vec<CommandRecord>,
    pub spec: ScheduleSpec,
    pub warnings: Vec<String>,
    /// Unit files from a previous install of a job this install no longer
    /// registers (e.g. `--no-nightly` after a full install): unloaded from
    /// the scheduler and removed so no orphaned job keeps firing.
    pub stale_removed: Vec<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UninstallReport {
    pub platform: Platform,
    pub dry_run: bool,
    pub removed: Vec<PathBuf>,
    pub missing: Vec<PathBuf>,
    pub commands: Vec<CommandRecord>,
}

#[derive(Debug)]
pub enum ScheduleError {
    Unsupported(String),
    Io(String),
    Command(String),
}

impl std::fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(msg) | Self::Io(msg) | Self::Command(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ScheduleError {}

fn run_recorded(argv: &[String], execute: bool) -> CommandRecord {
    if !execute {
        return CommandRecord {
            argv: argv.to_vec(),
            executed: false,
            exit_code: None,
            stderr: None,
        };
    }
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(output) => CommandRecord {
            argv: argv.to_vec(),
            executed: true,
            exit_code: output.status.code(),
            stderr: {
                let s = String::from_utf8_lossy(&output.stderr).trim().to_string();
                if s.is_empty() { None } else { Some(s) }
            },
        },
        Err(error) => CommandRecord {
            argv: argv.to_vec(),
            executed: true,
            exit_code: None,
            stderr: Some(error.to_string()),
        },
    }
}

fn current_uid() -> Option<String> {
    let output = Command::new("id").arg("-u").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uid.is_empty() { None } else { Some(uid) }
}

fn home_dir() -> Result<PathBuf, ScheduleError> {
    dirs::home_dir().ok_or_else(|| ScheduleError::Io("cannot resolve home directory".into()))
}

fn xdg_config_home() -> Option<PathBuf> {
    dotenvy::var("XDG_CONFIG_HOME")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn write_unit(unit: &UnitFile) -> Result<(), ScheduleError> {
    if let Some(parent) = unit.path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ScheduleError::Io(format!("cannot create {}: {e}", parent.display())))?;
    }
    let tmp = unit.path.with_extension("tmp");
    std::fs::write(&tmp, &unit.contents)
        .map_err(|e| ScheduleError::Io(format!("cannot write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, &unit.path)
        .map_err(|e| ScheduleError::Io(format!("cannot publish {}: {e}", unit.path.display())))
}

/// Register the jobs with the OS scheduler (or describe what would happen).
pub fn install(spec: &ScheduleSpec, dry_run: bool) -> Result<InstallReport, ScheduleError> {
    let platform = Platform::detect();
    let home = home_dir()?;
    let Some(unit_dir) = unit_dir(platform, &home, xdg_config_home().as_deref()) else {
        return Err(ScheduleError::Unsupported(format!(
            "cass schedule is not supported on this platform ({}); register `{} schedule run --job incremental --data-dir {}` with your scheduler (e.g. Task Scheduler / schtasks) manually",
            std::env::consts::OS,
            spec.binary.display(),
            spec.data_dir.display()
        )));
    };
    let units = render_units(spec, platform, &unit_dir);
    let mut commands = Vec::new();
    let mut warnings = Vec::new();

    if !dry_run {
        std::fs::create_dir_all(schedule_dir(&spec.data_dir)).map_err(|e| {
            ScheduleError::Io(format!(
                "cannot create {}: {e}",
                schedule_dir(&spec.data_dir).display()
            ))
        })?;
        for unit in &units {
            write_unit(unit)?;
        }
    }

    // De-register jobs this install no longer selects (e.g. `--no-nightly`
    // after a previous full install). Without this, the old nightly unit
    // would keep firing forever with no surface reporting it.
    let mut stale_removed = Vec::new();
    let stale_jobs: Vec<ScheduleJob> = [ScheduleJob::Incremental, ScheduleJob::Nightly]
        .into_iter()
        .filter(|job| !spec.jobs().contains(job))
        .collect();
    for job in &stale_jobs {
        let stale_paths: Vec<PathBuf> = match platform {
            Platform::Launchd => vec![unit_dir.join(format!("{}.plist", job.launchd_label()))],
            Platform::Systemd => {
                let base = job.systemd_unit_base();
                vec![
                    unit_dir.join(format!("{base}.timer")),
                    unit_dir.join(format!("{base}.service")),
                ]
            }
            Platform::Unsupported => Vec::new(),
        };
        let previously_installed = stale_paths.iter().any(|p| p.exists());
        if !previously_installed {
            continue;
        }
        match platform {
            Platform::Launchd => {
                let uid = current_uid().unwrap_or_else(|| "501".to_string());
                commands.push(run_recorded(
                    &[
                        "launchctl".to_string(),
                        "bootout".to_string(),
                        format!("gui/{uid}/{}", job.launchd_label()),
                    ],
                    !dry_run,
                ));
            }
            Platform::Systemd => {
                commands.push(run_recorded(
                    &[
                        "systemctl".to_string(),
                        "--user".to_string(),
                        "disable".to_string(),
                        "--now".to_string(),
                        format!("{}.timer", job.systemd_unit_base()),
                    ],
                    !dry_run,
                ));
            }
            Platform::Unsupported => {}
        }
        for path in stale_paths {
            if path.exists() {
                if !dry_run {
                    std::fs::remove_file(&path).map_err(|e| {
                        ScheduleError::Io(format!("cannot remove {}: {e}", path.display()))
                    })?;
                }
                stale_removed.push(path);
            }
        }
    }

    match platform {
        Platform::Launchd => {
            let uid = current_uid().unwrap_or_else(|| "501".to_string());
            for job in spec.jobs() {
                let label = job.launchd_label();
                let plist = unit_dir.join(format!("{label}.plist"));
                // bootout is best-effort: it fails when the job was never loaded.
                let bootout = vec![
                    "launchctl".to_string(),
                    "bootout".to_string(),
                    format!("gui/{uid}/{label}"),
                ];
                commands.push(run_recorded(&bootout, !dry_run));
                let bootstrap = vec![
                    "launchctl".to_string(),
                    "bootstrap".to_string(),
                    format!("gui/{uid}"),
                    plist.display().to_string(),
                ];
                let record = run_recorded(&bootstrap, !dry_run);
                if record.executed && record.exit_code != Some(0) {
                    return Err(ScheduleError::Command(format!(
                        "launchctl bootstrap failed for {label}: {}",
                        record.stderr.clone().unwrap_or_default()
                    )));
                }
                commands.push(record);
            }
        }
        Platform::Systemd => {
            let reload = vec![
                "systemctl".to_string(),
                "--user".to_string(),
                "daemon-reload".to_string(),
            ];
            let record = run_recorded(&reload, !dry_run);
            if record.executed && record.exit_code != Some(0) {
                return Err(ScheduleError::Command(format!(
                    "systemctl --user daemon-reload failed: {}",
                    record.stderr.clone().unwrap_or_default()
                )));
            }
            commands.push(record);
            let mut enable = vec![
                "systemctl".to_string(),
                "--user".to_string(),
                "enable".to_string(),
                "--now".to_string(),
            ];
            enable.extend(
                spec.jobs()
                    .into_iter()
                    .map(|job| format!("{}.timer", job.systemd_unit_base())),
            );
            let record = run_recorded(&enable, !dry_run);
            if record.executed && record.exit_code != Some(0) {
                return Err(ScheduleError::Command(format!(
                    "systemctl --user enable --now failed: {}",
                    record.stderr.clone().unwrap_or_default()
                )));
            }
            commands.push(record);
            warnings.push(
                "systemd user timers only run while a user session exists; run `loginctl enable-linger $USER` once so they also run when you are logged out".to_string(),
            );
        }
        Platform::Unsupported => {}
    }

    Ok(InstallReport {
        platform,
        dry_run,
        unit_dir: Some(unit_dir),
        units,
        commands,
        spec: spec.clone(),
        warnings,
        stale_removed,
    })
}

/// Unregister the jobs and remove the unit files cass wrote.
pub fn uninstall(dry_run: bool) -> Result<UninstallReport, ScheduleError> {
    let platform = Platform::detect();
    let home = home_dir()?;
    let Some(unit_dir) = unit_dir(platform, &home, xdg_config_home().as_deref()) else {
        return Err(ScheduleError::Unsupported(format!(
            "cass schedule is not supported on this platform ({})",
            std::env::consts::OS
        )));
    };
    let mut commands = Vec::new();
    let mut removed = Vec::new();
    let mut missing = Vec::new();
    let jobs = [ScheduleJob::Incremental, ScheduleJob::Nightly];
    let mut candidate_paths = Vec::new();
    match platform {
        Platform::Launchd => {
            let uid = current_uid().unwrap_or_else(|| "501".to_string());
            for job in jobs {
                let label = job.launchd_label();
                let bootout = vec![
                    "launchctl".to_string(),
                    "bootout".to_string(),
                    format!("gui/{uid}/{label}"),
                ];
                commands.push(run_recorded(&bootout, !dry_run));
                candidate_paths.push(unit_dir.join(format!("{label}.plist")));
            }
        }
        Platform::Systemd => {
            let mut disable = vec![
                "systemctl".to_string(),
                "--user".to_string(),
                "disable".to_string(),
                "--now".to_string(),
            ];
            disable.extend(
                jobs.iter()
                    .map(|job| format!("{}.timer", job.systemd_unit_base())),
            );
            commands.push(run_recorded(&disable, !dry_run));
            for job in jobs {
                let base = job.systemd_unit_base();
                candidate_paths.push(unit_dir.join(format!("{base}.timer")));
                candidate_paths.push(unit_dir.join(format!("{base}.service")));
            }
        }
        Platform::Unsupported => {}
    }
    for path in candidate_paths {
        if path.exists() {
            if !dry_run {
                std::fs::remove_file(&path).map_err(|e| {
                    ScheduleError::Io(format!("cannot remove {}: {e}", path.display()))
                })?;
            }
            removed.push(path);
        } else {
            missing.push(path);
        }
    }
    if matches!(platform, Platform::Systemd) {
        commands.push(run_recorded(
            &[
                "systemctl".to_string(),
                "--user".to_string(),
                "daemon-reload".to_string(),
            ],
            !dry_run,
        ));
    }
    Ok(UninstallReport {
        platform,
        dry_run,
        removed,
        missing,
        commands,
    })
}

// ---------------------------------------------------------------------------
// Job execution
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepReport {
    pub name: String,
    pub argv: Vec<String>,
    pub exit_code: Option<i32>,
    pub ok: bool,
    pub duration_ms: u64,
    pub skipped_reason: Option<String>,
    /// Last JSON document the step printed to stdout, when parseable.
    pub result: Option<serde_json::Value>,
    pub stderr_tail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobReport {
    pub job: ScheduleJob,
    pub started_ms: i64,
    pub finished_ms: i64,
    pub ok: bool,
    pub skipped_reason: Option<String>,
    pub steps: Vec<StepReport>,
    pub pressure: Option<serde_json::Value>,
    pub user_idle: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScheduleState {
    pub last_incremental: Option<JobReport>,
    pub last_nightly: Option<JobReport>,
}

pub fn load_state(data_dir: &Path) -> ScheduleState {
    let state = std::fs::read_to_string(state_path(data_dir))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    outcomes::normalize_state(state)
}

fn save_state(data_dir: &Path, state: &ScheduleState) -> std::io::Result<()> {
    let path = state_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    let mut file = File::create(&tmp)?;
    file.write_all(&body)?;
    file.sync_all()?;
    std::fs::rename(&tmp, &path)
}

fn append_run(data_dir: &Path, report: &JobReport) -> std::io::Result<()> {
    let path = runs_log_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(report).map_err(std::io::Error::other)?;
    writeln!(file, "{line}")?;
    file.sync_all()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Runtime inputs for `schedule run`.
#[derive(Clone, Debug)]
pub struct RunConfig {
    pub binary: PathBuf,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub semantic: bool,
    pub max_backfill_batches: u32,
}

fn last_json_document(stdout: &str) -> Option<serde_json::Value> {
    // Steps print exactly one JSON document, but tolerate leading noise by
    // taking the last line that parses (compact) or the whole buffer (pretty).
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        return Some(value);
    }
    stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
}

fn run_step(
    name: &str,
    binary: &Path,
    data_dir: &Path,
    args: &[String],
    environment: &[(&str, &str)],
    log: Option<&mut File>,
) -> StepReport {
    let started = Instant::now();
    let mut argv = vec![binary.display().to_string()];
    argv.extend(args.iter().cloned());
    // ubs:ignore — `binary` is the absolute `std::env::current_exe()` path
    // resolved by the CLI layer, never user-supplied input.
    //
    // CASS_DATA_DIR is exported to every step because some child commands
    // (`sources sync`, `models status`) have no `--data-dir` flag and fall
    // back to the platform default otherwise — which would silently target
    // the wrong archive when `schedule run --data-dir` names a custom one.
    let mut command = Command::new(binary);
    command
        .args(args)
        .envs(environment.iter().copied())
        .stdin(Stdio::null())
        .env("CASS_INDEX_NO_PROGRESS_EVENTS", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_DATA_DIR", data_dir);
    crate::indexer::background_refresh::cap_malloc_arenas_for_background_child(&mut command);
    let output = command.output();
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            if let Some(log) = log {
                let _ = writeln!(log, "== step {name}: {}", argv.join(" "));
                let _ = log.write_all(stdout.as_bytes());
                let _ = log.write_all(stderr.as_bytes());
            }
            let exit_code = output.status.code();
            StepReport {
                name: name.to_string(),
                argv,
                exit_code,
                ok: output.status.success(),
                duration_ms,
                skipped_reason: None,
                result: last_json_document(&stdout),
                stderr_tail: stderr_tail(&stderr),
            }
        }
        Err(error) => StepReport {
            name: name.to_string(),
            argv,
            exit_code: None,
            ok: false,
            duration_ms,
            skipped_reason: None,
            result: None,
            stderr_tail: Some(error.to_string()),
        },
    }
}

fn stderr_tail(stderr: &str) -> Option<String> {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        return None;
    }
    let tail: Vec<&str> = trimmed.lines().rev().take(20).collect();
    Some(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
}

fn skipped_step(name: &str, reason: impl Into<String>) -> StepReport {
    StepReport {
        name: name.to_string(),
        argv: Vec::new(),
        exit_code: None,
        ok: true,
        duration_ms: 0,
        skipped_reason: Some(reason.into()),
        result: None,
        stderr_tail: None,
    }
}

/// Decide whether a scheduled job should run now, and why not.
pub fn job_gate(job: ScheduleJob) -> Option<String> {
    let pressure = responsiveness::machine_pressure_now();
    if pressure.severe {
        return Some(format!(
            "machine under severe load (load/core={:?}, psi={:?}); skipping this run",
            pressure.load_per_core, pressure.psi_cpu_some_avg10
        ));
    }
    if matches!(job, ScheduleJob::Nightly) {
        let gate = responsiveness::user_idle_gate();
        if !gate.satisfied {
            return Some(format!(
                "console user active (idle {:?}s < required {}s); skipping nightly run",
                gate.observed_secs, gate.required_secs
            ));
        }
    }
    None
}

/// Execute one job end-to-end. Admitted runs persist their report, treating
/// persistence errors as failures. Admission refusals do not clobber an owner.
pub fn run_job(job: ScheduleJob, cfg: &RunConfig) -> JobReport {
    run_job_with_gate(job, cfg, job_gate(job))
}

/// `--force`: run even when the load / console-idle gates would skip.
pub fn run_job_unconditionally(job: ScheduleJob, cfg: &RunConfig) -> JobReport {
    run_job_with_gate(job, cfg, None)
}

fn run_job_with_gate(
    job: ScheduleJob,
    cfg: &RunConfig,
    skipped_reason: Option<String>,
) -> JobReport {
    // Hold this distinct scheduler lease through source sync, all child work,
    // and receipt publication. The child's index lock alone is not enough.
    let _lease = match execution::acquire(&cfg.data_dir) {
        Ok(lease) => lease,
        Err(error) => return error.report(job),
    };
    let started_ms = now_ms();
    let mut steps = Vec::new();
    let pressure = serde_json::to_value(responsiveness::machine_pressure_now()).ok();
    let user_idle = serde_json::to_value(responsiveness::user_idle_gate()).ok();

    let mut log = {
        let path = job_log_path(&cfg.data_dir, job);
        std::fs::create_dir_all(schedule_dir(&cfg.data_dir)).ok();
        OpenOptions::new().create(true).append(true).open(path).ok()
    };
    if let Some(log) = log.as_mut() {
        let _ = writeln!(
            log,
            "\n=== cass schedule run --job {} @ {} ===",
            job.as_str(),
            chrono::Utc::now().to_rfc3339()
        );
    }

    if skipped_reason.is_none() {
        // 1. Remote syncs that are due.
        let discovery_started = Instant::now();
        let due = match due_remote_sources(&cfg.data_dir, started_ms) {
            Ok(due) => due,
            Err(error) => {
                steps.push(StepReport {
                    name: "sources-discovery".to_string(),
                    argv: Vec::new(),
                    exit_code: None,
                    ok: false,
                    duration_ms: u64::try_from(discovery_started.elapsed().as_millis())
                        .unwrap_or(u64::MAX),
                    skipped_reason: None,
                    result: Some(serde_json::json!({
                        "status": "failed",
                        "reason": error.reason_code(),
                        "dependent_work_started": false,
                    })),
                    stderr_tail: Some(error.to_string()),
                });
                // No sync, index, or model process may start from an unknown
                // source plan. Persist the failure, not fabricated freshness.
                return persist_job_report(
                    cfg,
                    JobReport {
                        job,
                        started_ms,
                        finished_ms: now_ms(),
                        ok: false,
                        skipped_reason: None,
                        steps,
                        pressure,
                        user_idle,
                    },
                );
            }
        };
        if due.is_empty() {
            steps.push(skipped_step("sources-sync", "no remote source is due"));
        } else {
            for (name, why) in due {
                let args = vec![
                    "--color=never".to_string(),
                    "sources".to_string(),
                    "sync".to_string(),
                    "--source".to_string(),
                    name.clone(),
                    "--no-index".to_string(),
                    "--json".to_string(),
                ];
                info!(source = %name, reason = %why, "scheduled remote sync");
                steps.push(run_step(
                    &format!("sources-sync:{name}"),
                    &cfg.binary,
                    &cfg.data_dir,
                    &args,
                    &[],
                    log.as_mut(),
                ));
            }
        }

        // 2. Index. Exit 7 is an expected lock-contention deferral, but no
        // indexing completed. Preserve the non-success outcome so repeated
        // contention cannot masquerade as refreshed history; the next timer
        // firing still retries normally.
        let full = matches!(job, ScheduleJob::Nightly);
        // Retain the full source census: timestamp-only connectors can miss
        // restored historical files. The indexer may reconcile a verified
        // lexical generation inline instead of rebuilding the whole archive.
        let index_environment: &[(&str, &str)] = if full {
            &[("CASS_NIGHTLY_RECONCILIATION", "1")]
        } else {
            &[]
        };
        let args: Vec<String> = background_index_args(&cfg.data_dir, &cfg.db_path, full)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let mut index_step = run_step(
            if full { "index-full" } else { "index" },
            &cfg.binary,
            &cfg.data_dir,
            &args,
            index_environment,
            log.as_mut(),
        );
        let index_busy = soften_busy_index_step(&mut index_step);
        steps.push(index_step);
        let prerequisites_ok = steps.iter().all(|step| step.ok);

        // 3. Semantic backfill (nightly only). Two tiers: the fast (hash)
        // tier needs no model files; the quality (MiniLM) tier only runs
        // when the model is installed — requesting it without the model
        // fails with a model-domain exit code, which must read as "tier not
        // available", never as a failed nightly.
        if full {
            if !cfg.semantic {
                steps.push(skipped_step(
                    "semantic-backfill",
                    "disabled with --no-semantic",
                ));
            } else if index_busy {
                steps.push(skipped_step(
                    "semantic-backfill",
                    "index run was busy; leaving semantic backfill for the next night",
                ));
            } else if !prerequisites_ok {
                steps.push(skipped_step(
                    "semantic-backfill",
                    "source sync or indexing failed; semantic backfill requires a successful run",
                ));
            } else {
                let (probe, model_installed) =
                    minilm_model_probe(&cfg.binary, &cfg.data_dir, log.as_mut());
                steps.push(probe);
                let mut tiers = vec!["fast"];
                if model_installed {
                    tiers.push("quality");
                } else {
                    steps.push(skipped_step(
                        "semantic-backfill:quality",
                        "MiniLM model not installed (`cass models install`); quality tier skipped",
                    ));
                }
                let mut batches = 0u32;
                for tier in tiers {
                    let remaining = cfg.max_backfill_batches - batches;
                    if remaining == 0 {
                        steps.push(skipped_step(
                            "semantic-backfill",
                            format!("stopped after {batches} batches (CASS_SCHEDULE_MAX_BACKFILL_BATCHES)"),
                        ));
                        break;
                    }
                    let args = vec![
                        "--db".to_string(),
                        cfg.db_path.display().to_string(),
                        "--color=never".to_string(),
                        "models".to_string(),
                        "backfill".to_string(),
                        "--tier".to_string(),
                        tier.to_string(),
                        "--max-batches".to_string(),
                        remaining.to_string(),
                        "--scheduled".to_string(),
                        "--data-dir".to_string(),
                        cfg.data_dir.display().to_string(),
                        "--json".to_string(),
                    ];
                    let mut step = run_step(
                        &format!("semantic-backfill:{tier}:{}", batches + 1),
                        &cfg.binary,
                        &cfg.data_dir,
                        &args,
                        &[],
                        log.as_mut(),
                    );
                    let model_unavailable = soften_model_unavailable_backfill_step(&mut step);
                    if model_unavailable || !step.ok {
                        steps.push(step);
                        break;
                    }
                    let Some(attempted) = backfill_worker_attempts(&step, remaining) else {
                        step.ok = false;
                        step.stderr_tail = Some(
                            "backfill worker returned missing or invalid batch accounting; no further tiers were launched"
                                .to_string(),
                        );
                        steps.push(step);
                        break;
                    };
                    batches += attempted;
                    let paused = step.result.as_ref().is_some_and(|result| {
                        matches!(
                            result.get("status").and_then(serde_json::Value::as_str),
                            Some("paused" | "disabled")
                        )
                    });
                    steps.push(step);
                    if paused {
                        break;
                    }
                }
            }
        }
    }

    let ok = steps.iter().all(|s| s.ok);
    let report = JobReport {
        job,
        started_ms,
        finished_ms: now_ms(),
        ok,
        skipped_reason,
        steps,
        pressure,
        user_idle,
    };
    persist_job_report(cfg, report)
}

fn persist_job_report(cfg: &RunConfig, mut report: JobReport) -> JobReport {
    let mut state = load_state(&cfg.data_dir);
    match report.job {
        ScheduleJob::Incremental => state.last_incremental = Some(report.clone()),
        ScheduleJob::Nightly => state.last_nightly = Some(report.clone()),
    }
    let state_saved = match save_state(&cfg.data_dir, &state) {
        Ok(()) => true,
        Err(error) => {
            warn!(kind = ?error.kind(), "failed to persist schedule state");
            record_persistence_failure(&mut report, "schedule_state_write_failed");
            false
        }
    };
    if let Err(error) = append_run(&cfg.data_dir, &report) {
        warn!(kind = ?error.kind(), "failed to append schedule run history");
        record_persistence_failure(&mut report, "schedule_history_write_failed");
        // Do not leave a successful state receipt after history publication
        // failed. Retry only this bounded correction, still under the lease.
        if state_saved {
            match report.job {
                ScheduleJob::Incremental => state.last_incremental = Some(report.clone()),
                ScheduleJob::Nightly => state.last_nightly = Some(report.clone()),
            }
            if let Err(error) = save_state(&cfg.data_dir, &state) {
                warn!(kind = ?error.kind(), "failed to persist corrected schedule state");
                record_persistence_failure(&mut report, "schedule_state_write_failed");
            }
        }
    }
    report
}

fn record_persistence_failure(report: &mut JobReport, reason: &str) {
    report.ok = false;
    report.steps.push(StepReport {
        name: "schedule-persistence".to_string(),
        argv: Vec::new(),
        exit_code: None,
        ok: false,
        duration_ms: 0,
        skipped_reason: None,
        result: Some(serde_json::json!({"status": "failed", "reason": reason})),
        stderr_tail: None,
    });
}

/// Exit codes that mean "the semantic model / embedder is unavailable"
/// rather than "the batch failed": 15 (semantic/embedder unavailable) and
/// 20–24 (model acquisition, verify, and model-handling I/O domain — see the
/// exit-code table in AGENTS.md).
const MODEL_UNAVAILABLE_EXIT_CODES: [i32; 6] = [15, 20, 21, 22, 23, 24];

/// Convert a backfill batch that failed because the model is unavailable
/// into a skipped (non-failing) step. Returns true when the conversion
/// applied, which also ends that tier's loop.
pub fn soften_model_unavailable_backfill_step(step: &mut StepReport) -> bool {
    if step.ok {
        return false;
    }
    let Some(code) = step.exit_code else {
        return false;
    };
    if !MODEL_UNAVAILABLE_EXIT_CODES.contains(&code) {
        return false;
    }
    step.ok = true;
    step.skipped_reason = Some(format!(
        "semantic model unavailable (exit {code}); run `cass models install` to enable this tier"
    ));
    true
}

/// Annotate exit-7 (`index-busy`) as a deferred step without claiming that
/// indexing succeeded. The next timer firing retries, but callers and saved
/// receipts must distinguish that retryable deferral from completed work.
pub fn soften_busy_index_step(step: &mut StepReport) -> bool {
    if step.exit_code != Some(7) {
        return false;
    }
    step.ok = false;
    // Exit 7 also covers a background run that deferred the one-time storage
    // migration repair (GH #450); name the cause the child reported.
    step.skipped_reason = Some(
        if step_error_kind(step).as_deref() == Some("migration-repair-pending") {
            "the archive still needs its one-time storage migration repair; run `cass index --full` in the foreground".to_string()
        } else {
            "another index run already holds the index lock; skipped this cycle".to_string()
        },
    );
    true
}

/// `error.kind` from the last JSON error envelope a step wrote to stderr.
fn step_error_kind(step: &StepReport) -> Option<String> {
    step.stderr_tail.as_deref()?.lines().rev().find_map(|line| {
        serde_json::from_str::<serde_json::Value>(line.trim())
            .ok()?
            .pointer("/error/kind")?
            .as_str()
            .map(str::to_owned)
    })
}

/// Probe whether the MiniLM model is installed via `models status --json`.
/// A failed probe is downgraded to "assume not installed" so it can never
/// fail the nightly job on its own.
fn minilm_model_probe(
    binary: &Path,
    data_dir: &Path,
    log: Option<&mut File>,
) -> (StepReport, bool) {
    let args = vec![
        "--color=never".to_string(),
        "models".to_string(),
        "status".to_string(),
        "--json".to_string(),
    ];
    let mut step = run_step("models-status", binary, data_dir, &args, &[], log);
    let installed = step
        .result
        .as_ref()
        .and_then(|r| r.get("installed"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !step.ok {
        step.ok = true;
        step.skipped_reason = Some(
            "models status probe failed; assuming the MiniLM model is not installed".to_string(),
        );
    }
    (step, installed)
}

/// Accept only exact, bounded worker accounting before spending the remainder
/// of the global nightly allowance on another tier.
fn backfill_worker_attempts(step: &StepReport, remaining: u32) -> Option<u32> {
    if !step.ok {
        return None;
    }
    let result = step.result.as_ref()?;
    let attempted = u32::try_from(result.get("batches_attempted")?.as_u64()?).ok()?;
    let completed = u32::try_from(result.get("batches_completed")?.as_u64()?).ok()?;
    let uncompleted = attempted.checked_sub(completed)?;
    let initializations = result.get("model_initializations")?.as_u64()?;
    let status = result.get("status")?.as_str()?;
    let admitted = matches!(status, "checkpointed" | "published" | "unchanged" | "idle");
    (attempted > 0
        && attempted <= remaining
        && initializations == u64::from(completed > 0)
        && (admitted || matches!(status, "paused" | "disabled"))
        && uncompleted == u32::from(!admitted))
    .then_some(attempted)
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct UnitStatus {
    pub job: ScheduleJob,
    pub unit_files: Vec<PathBuf>,
    pub installed: bool,
    /// Scheduler-reported state, when the platform tool answered.
    pub loaded: Option<bool>,
    /// Last persisted run outcome, followed by explicitly labeled probe status.
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusReport {
    pub platform: Platform,
    pub unit_dir: Option<PathBuf>,
    pub units: Vec<UnitStatus>,
    pub state: ScheduleState,
    pub auto_refresh: Option<crate::indexer::background_refresh::AutoRefreshState>,
    pub log_dir: PathBuf,
}

pub fn status(data_dir: &Path) -> StatusReport {
    let platform = Platform::detect();
    let unit_dir = home_dir()
        .ok()
        .and_then(|home| unit_dir(platform, &home, xdg_config_home().as_deref()));
    let state = load_state(data_dir);
    let mut units = Vec::new();
    if let Some(dir) = &unit_dir {
        let uid = current_uid();
        for job in [ScheduleJob::Incremental, ScheduleJob::Nightly] {
            let (files, probe): (Vec<PathBuf>, Option<Vec<String>>) = match platform {
                Platform::Launchd => (
                    vec![dir.join(format!("{}.plist", job.launchd_label()))],
                    uid.as_ref().map(|uid| {
                        vec![
                            "launchctl".to_string(),
                            "print".to_string(),
                            format!("gui/{uid}/{}", job.launchd_label()),
                        ]
                    }),
                ),
                Platform::Systemd => (
                    vec![
                        dir.join(format!("{}.timer", job.systemd_unit_base())),
                        dir.join(format!("{}.service", job.systemd_unit_base())),
                    ],
                    Some(vec![
                        "systemctl".to_string(),
                        "--user".to_string(),
                        "is-active".to_string(),
                        format!("{}.timer", job.systemd_unit_base()),
                    ]),
                ),
                Platform::Unsupported => (Vec::new(), None),
            };
            let installed = !files.is_empty() && files.iter().all(|f| f.exists());
            let (loaded, detail) = match probe {
                Some(argv) if installed => {
                    let record = run_recorded(&argv, true);
                    (
                        record.exit_code.map(|c| c == 0),
                        record
                            .stderr
                            .or_else(|| record.exit_code.map(|c| format!("exit {c}"))),
                    )
                }
                _ => (None, None),
            };
            units.push(UnitStatus {
                job,
                unit_files: files,
                installed,
                loaded,
                detail: outcomes::unit_detail(job, &state, detail),
            });
        }
    }
    StatusReport {
        platform,
        unit_dir,
        units,
        state,
        auto_refresh: crate::indexer::background_refresh::load_state(data_dir),
        log_dir: schedule_dir(data_dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ScheduleSpec {
        ScheduleSpec {
            interval_mins: 15,
            nightly_hour: 3,
            nightly_minute: 30,
            nightly: true,
            semantic: true,
            binary: PathBuf::from("/usr/local/bin/cass"),
            data_dir: PathBuf::from("/home/u/.local/share/coding-agent-search"),
            db_path: None,
        }
    }

    #[test]
    fn job_args_pin_the_run_contract() {
        let s = spec();
        assert_eq!(
            s.job_args(ScheduleJob::Incremental),
            vec![
                "--color=never",
                "schedule",
                "run",
                "--job",
                "incremental",
                "--data-dir",
                "/home/u/.local/share/coding-agent-search",
                "--json",
            ]
        );
        let mut no_sem = spec();
        no_sem.semantic = false;
        no_sem.db_path = Some(PathBuf::from("/x/db.sqlite"));
        let args = no_sem.job_args(ScheduleJob::Nightly);
        assert_eq!(&args[..2], &["--db", "/x/db.sqlite"]);
        assert_eq!(args.last().unwrap(), "--no-semantic");
    }

    #[test]
    fn launchd_plist_renders_interval_and_calendar_triggers() {
        let s = spec();
        let inc = render_launchd_plist(&s, ScheduleJob::Incremental);
        assert!(inc.contains("<string>com.dicklesworthstone.cass.incremental</string>"));
        assert!(inc.contains("<key>StartInterval</key>\n    <integer>900</integer>"));
        assert!(inc.contains("<key>Nice</key>"));
        assert!(!inc.contains("<key>LowPriorityIO</key>"));
        assert!(inc.contains("/schedule/incremental.log</string>"));
        assert!(!inc.contains("StartCalendarInterval"));

        let night = render_launchd_plist(&s, ScheduleJob::Nightly);
        assert!(night.contains("<key>StartCalendarInterval</key>"));
        assert!(night.contains("<key>Hour</key>\n        <integer>3</integer>"));
        assert!(night.contains("<key>Minute</key>\n        <integer>30</integer>"));
        assert!(night.contains("<string>nightly</string>"));
    }

    #[test]
    fn launchd_jobs_keep_cpu_niceness_without_background_io_throttling() {
        for job in [ScheduleJob::Incremental, ScheduleJob::Nightly] {
            let plist = render_launchd_plist(&spec(), job);
            // The renderer's line-continuation literal drops indentation, so
            // check the key/value pairing structurally rather than by exact
            // whitespace: the value that follows `Nice` must be 15.
            let nice_at = plist
                .find("<key>Nice</key>")
                .unwrap_or_else(|| panic!("{} must set Nice: {plist}", job.as_str()));
            let after_nice = plist[nice_at + "<key>Nice</key>".len()..].trim_start();
            assert!(
                after_nice.starts_with("<integer>15</integer>"),
                "{} must keep Nice=15: {plist}",
                job.as_str()
            );
            for key in ["ProcessType", "LowPriorityIO", "LowPriorityBackgroundIO"] {
                assert!(
                    !plist.contains(&format!("<key>{key}</key>")),
                    "{} must not request {key}: {plist}",
                    job.as_str()
                );
            }
        }
    }

    #[test]
    fn launchd_plist_escapes_xml_in_paths() {
        let mut s = spec();
        s.data_dir = PathBuf::from("/tmp/a&b<c>");
        let plist = render_launchd_plist(&s, ScheduleJob::Incremental);
        assert!(plist.contains("/tmp/a&amp;b&lt;c&gt;"));
        assert!(!plist.contains("a&b<c>"));
    }

    #[test]
    fn systemd_units_render_idle_priority_and_timers() {
        let s = spec();
        let service = render_systemd_service(&s, ScheduleJob::Incremental);
        assert!(service.contains("Type=oneshot"));
        assert!(service.contains("Nice=19"));
        assert!(service.contains("IOSchedulingClass=idle"));
        assert!(service.contains("CPUSchedulingPolicy=idle"));
        assert!(service.contains(
            "ExecStart=/usr/local/bin/cass --color=never schedule run --job incremental"
        ));
        let timer = render_systemd_timer(&s, ScheduleJob::Incremental);
        assert!(timer.contains("OnUnitActiveSec=15min"));
        assert!(timer.contains("Persistent=true"));
        assert!(timer.contains("Unit=cass-incremental.service"));
        let nightly = render_systemd_timer(&s, ScheduleJob::Nightly);
        assert!(nightly.contains("OnCalendar=*-*-* 03:30:00"));
    }

    #[test]
    fn systemd_quote_wraps_paths_with_spaces() {
        assert_eq!(systemd_quote("/usr/bin/cass"), "/usr/bin/cass");
        assert_eq!(systemd_quote("/Users/me/My Data"), "\"/Users/me/My Data\"");
        assert_eq!(systemd_quote("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn render_units_respects_nightly_toggle_and_platform() {
        let dir = Path::new("/units");
        let with = render_units(&spec(), Platform::Launchd, dir);
        assert_eq!(with.len(), 2);
        assert_eq!(
            with[0].path,
            dir.join("com.dicklesworthstone.cass.incremental.plist")
        );
        let mut no_night = spec();
        no_night.nightly = false;
        assert_eq!(render_units(&no_night, Platform::Launchd, dir).len(), 1);
        assert_eq!(render_units(&spec(), Platform::Systemd, dir).len(), 4);
        assert!(render_units(&spec(), Platform::Unsupported, dir).is_empty());
    }

    #[test]
    fn unit_dir_follows_platform_conventions() {
        let home = Path::new("/home/u");
        assert_eq!(
            unit_dir(Platform::Launchd, home, None).unwrap(),
            PathBuf::from("/home/u/Library/LaunchAgents")
        );
        assert_eq!(
            unit_dir(Platform::Systemd, home, None).unwrap(),
            PathBuf::from("/home/u/.config/systemd/user")
        );
        assert_eq!(
            unit_dir(Platform::Systemd, home, Some(Path::new("/xdg"))).unwrap(),
            PathBuf::from("/xdg/systemd/user")
        );
        assert!(unit_dir(Platform::Unsupported, home, None).is_none());
    }

    #[test]
    fn backfill_worker_accounting_rejects_missing_or_unbounded_counts() {
        let step = |ok: bool, result: Option<serde_json::Value>| StepReport {
            name: "b".into(),
            argv: vec![],
            exit_code: Some(if ok { 0 } else { 1 }),
            ok,
            duration_ms: 0,
            skipped_reason: None,
            result,
            stderr_tail: None,
        };
        assert_eq!(backfill_worker_attempts(&step(false, None), 2), None);
        assert_eq!(backfill_worker_attempts(&step(true, None), 2), None);
        let counts = |attempted: serde_json::Value, completed, models| {
            serde_json::json!({
                "status": if completed > 0 { "checkpointed" } else { "paused" },
                "batches_attempted": attempted,
                "batches_completed": completed,
                "model_initializations": models,
            })
        };
        for invalid in [
            counts(0.into(), 0, 0),
            counts(3.into(), 2, 1),
            counts(1.into(), 2, 1),
            counts(2.into(), 1, 1),
            counts(1.into(), 1, 0),
            counts(2.into(), 2, 2),
            counts(serde_json::json!(1.5), 1, 1),
            counts(serde_json::json!(-1), 0, 0),
            counts(serde_json::json!(u64::MAX), 0, 0),
        ] {
            assert_eq!(
                backfill_worker_attempts(&step(true, Some(invalid)), 2),
                None
            );
        }
        for valid in [counts(1.into(), 0, 0), counts(2.into(), 2, 1)] {
            let expected = valid["batches_attempted"].as_u64().unwrap() as u32;
            assert_eq!(
                backfill_worker_attempts(&step(true, Some(valid)), 2),
                Some(expected)
            );
        }
        for invalid_status in [serde_json::Value::Null, "failed".into(), "cancelled".into()] {
            let mut report = counts(1.into(), 1, 1);
            report["status"] = invalid_status;
            assert_eq!(backfill_worker_attempts(&step(true, Some(report)), 2), None);
        }
        for (attempted, completed, models) in [(2, 2, 1), (2, 0, 0), (1, 0, 1)] {
            let mut report = counts(attempted.into(), completed, models);
            report["status"] = "paused".into();
            assert_eq!(backfill_worker_attempts(&step(true, Some(report)), 2), None);
        }
        let mut paused_after_checkpoint = counts(2.into(), 1, 1);
        paused_after_checkpoint["status"] = "paused".into();
        assert_eq!(
            backfill_worker_attempts(&step(true, Some(paused_after_checkpoint)), 2),
            Some(2)
        );
    }

    #[test]
    fn model_unavailable_backfill_batches_soften_to_skips() {
        let step = |exit: Option<i32>, ok: bool| StepReport {
            name: "b".into(),
            argv: vec![],
            exit_code: exit,
            ok,
            duration_ms: 0,
            skipped_reason: None,
            result: None,
            stderr_tail: None,
        };
        for code in [15, 20, 21, 22, 23, 24] {
            let mut s = step(Some(code), false);
            assert!(soften_model_unavailable_backfill_step(&mut s), "{code}");
            assert!(s.ok);
            assert!(
                s.skipped_reason
                    .as_deref()
                    .unwrap()
                    .contains("models install")
            );
        }
        let mut real_failure = step(Some(1), false);
        assert!(!soften_model_unavailable_backfill_step(&mut real_failure));
        assert!(!real_failure.ok);
        let mut spawn_failure = step(None, false);
        assert!(!soften_model_unavailable_backfill_step(&mut spawn_failure));
        let mut fine = step(Some(0), true);
        assert!(!soften_model_unavailable_backfill_step(&mut fine));
        assert!(fine.skipped_reason.is_none());
    }

    #[test]
    fn busy_index_step_is_deferred_without_claiming_success() {
        let step = |exit: Option<i32>, ok: bool| StepReport {
            name: "index".into(),
            argv: vec![],
            exit_code: exit,
            ok,
            duration_ms: 0,
            skipped_reason: None,
            result: None,
            stderr_tail: None,
        };
        let mut busy = step(Some(7), false);
        assert!(soften_busy_index_step(&mut busy));
        assert!(!busy.ok);
        assert!(busy.skipped_reason.is_some());
        let mut fine = step(Some(0), true);
        assert!(!soften_busy_index_step(&mut fine));
        let mut failed = step(Some(3), false);
        assert!(!soften_busy_index_step(&mut failed));
        assert!(!failed.ok);

        // GH #450: exit 7 from a deferred migration repair names that cause;
        // lock contention keeps its own reason.
        let with_stderr = |stderr: &str| StepReport {
            stderr_tail: Some(stderr.to_string()),
            ..step(Some(7), false)
        };
        let mut deferred = with_stderr(
            "note: something\n{\"error\":{\"code\":7,\"kind\":\"migration-repair-pending\",\"retryable\":false}}",
        );
        assert!(soften_busy_index_step(&mut deferred));
        assert!(!deferred.ok);
        let reason = deferred.skipped_reason.unwrap();
        assert!(reason.contains("migration repair"), "{reason}");
        assert!(reason.contains("cass index --full"), "{reason}");
        let mut locked =
            with_stderr("{\"error\":{\"code\":7,\"kind\":\"index-busy\",\"retryable\":true}}");
        assert!(soften_busy_index_step(&mut locked));
        assert!(locked.skipped_reason.unwrap().contains("index lock"));
    }

    #[test]
    fn last_json_document_handles_pretty_and_noisy_output() {
        let pretty = "{\n  \"a\": 1\n}\n";
        assert_eq!(last_json_document(pretty).unwrap()["a"], 1);
        let noisy = "warning: x\n{\"a\":1}\n{\"a\":2}\n";
        assert_eq!(last_json_document(noisy).unwrap()["a"], 2);
        assert!(last_json_document("nothing").is_none());
    }

    #[test]
    fn state_round_trips_and_history_appends() {
        let dir = tempfile::tempdir().unwrap();
        let report = JobReport {
            job: ScheduleJob::Incremental,
            started_ms: 1,
            finished_ms: 2,
            ok: true,
            skipped_reason: None,
            steps: vec![skipped_step("x", "why")],
            pressure: None,
            user_idle: None,
        };
        let state = ScheduleState {
            last_incremental: Some(report.clone()),
            last_nightly: None,
        };
        save_state(dir.path(), &state).unwrap();
        append_run(dir.path(), &report).unwrap();
        append_run(dir.path(), &report).unwrap();
        let loaded = load_state(dir.path());
        assert_eq!(loaded.last_incremental.unwrap().finished_ms, 2);
        assert!(loaded.last_nightly.is_none());
        let history = std::fs::read_to_string(runs_log_path(dir.path())).unwrap();
        assert_eq!(history.lines().count(), 2);
    }

    #[test]
    fn run_job_with_a_missing_binary_records_failure_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = RunConfig {
            binary: dir.path().join("definitely-not-cass"),
            data_dir: dir.path().to_path_buf(),
            db_path: dir.path().join("agent_search.db"),
            semantic: false,
            max_backfill_batches: 1,
        };
        let report = run_job(ScheduleJob::Incremental, &cfg);
        if report.skipped_reason.is_none() {
            assert!(!report.ok);
            let index = report.steps.iter().find(|s| s.name == "index").unwrap();
            assert!(!index.ok);
            assert!(index.stderr_tail.is_some());
        }
        assert!(state_path(dir.path()).exists());
        assert!(runs_log_path(dir.path()).exists());
    }
}
