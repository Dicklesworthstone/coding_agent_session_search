//! Revalidate positive session-writer observations without rescanning every file.
//!
//! The initial census is still a best-effort activity heuristic, not a writer
//! lock or an immutable source snapshot. On Linux, a cached positive may be
//! cleared only by a fresh census covering the initial visibility scope and
//! every previously observed writer owner: checking just the
//! old descriptor misses a writer that reopened through another descriptor or
//! process. Initially opaque processes/descriptors remain outside that heuristic
//! scope, bound to process start time; a new inspection gap never grants access.
//! In particular, this does not prove absence of writers we could never inspect.
//! A missing owner may be hidden by procfs policy, so even a genuinely departed
//! owner keeps cached positives deferred until a new filter is captured.
//! Linux keeps descriptor witnesses so a still-open writer costs only a few
//! procfs reads.
//! One refresh replaces evidence for every source, amortizing closed writers.
//! A newly incomplete refresh retains all prior positives for the filter's life.
//! macOS lsof can silently omit inaccessible processes; its captured positives
//! therefore remain deferred for the entire filter lifetime without reinspection.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::collections::HashSet;
use std::path::Path;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::Mutex;

type FileIdentity = (u64, u64);
type SourcePredicate = fn(&Path) -> bool;

#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
pub(crate) struct SourceWriterProbe {
    state: Mutex<ProbeState>,
    is_session_source: Option<SourcePredicate>,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
pub(crate) struct SourceWriterProbe {
    writers: HashMap<FileIdentity, Vec<(u32, u32)>>,
}

#[cfg(target_os = "macos")]
impl SourceWriterProbe {
    pub(crate) fn capture(is_session_source: SourcePredicate) -> Self {
        Self {
            writers: collect_writers(is_session_source),
        }
    }

    pub(crate) fn should_defer(&self, identity: FileIdentity) -> bool {
        // Apple lsof can skip a live process on EPERM without printing a
        // warning. Neither a successful census nor an empty targeted result
        // proves closure, so preserve the original conservative cache policy.
        self.writers.contains_key(&identity)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[derive(Debug, Default)]
pub(crate) struct SourceWriterProbe {}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl SourceWriterProbe {
    pub(crate) fn capture(_is_session_source: SourcePredicate) -> Self {
        Self {}
    }

    pub(crate) fn should_defer(&self, _identity: FileIdentity) -> bool {
        false
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct ProbeState {
    census: WriterCensus,
    initial_visibility_gaps: HashSet<VisibilityGap>,
    refresh_blocked: bool,
    #[cfg(test)]
    refresh_count: usize,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct WriterCensus {
    writers: HashMap<FileIdentity, Vec<DescriptorWitness>>,
    visibility_gaps: HashSet<VisibilityGap>,
    visible_processes: HashSet<ProcessIdentity>,
    complete: bool,
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
enum DescriptorWitness {
    Proc {
        descriptor: PathBuf,
        info: PathBuf,
        process: Option<ProcessIdentity>,
    },
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DescriptorState {
    Open,
    Closed,
    Uncertain,
}

#[cfg(target_os = "linux")]
impl SourceWriterProbe {
    pub(crate) fn capture(is_session_source: SourcePredicate) -> Self {
        let census = collect_writers(is_session_source, &HashMap::new());
        Self {
            state: Mutex::new(ProbeState {
                initial_visibility_gaps: census.visibility_gaps.clone(),
                census,
                ..ProbeState::default()
            }),
            is_session_source: Some(is_session_source),
        }
    }

    /// Query an identity already obtained from the source path by the caller.
    /// Read-only descriptors never supply positive writer evidence. Uncertain
    /// reinspection cannot revoke a previously observed positive.
    pub(crate) fn should_defer(&self, identity: FileIdentity) -> bool {
        let Some(is_session_source) = self.is_session_source else {
            return false;
        };
        let Ok(mut state) = self.state.lock() else {
            return true;
        };
        let Some(witnesses) = state.census.writers.get(&identity) else {
            return false;
        };
        for witness in witnesses {
            match witness.observe(identity) {
                DescriptorState::Open | DescriptorState::Uncertain => return true,
                DescriptorState::Closed => {}
            }
        }
        if state.refresh_blocked {
            return true;
        }

        // All cached descriptors closed or changed identity/access mode. A
        // successor can be anywhere in the process tree, so the old witnesses
        // alone never authorize reading. Do at most one census in this query.
        #[cfg(test)]
        {
            state.refresh_count += 1;
        }
        let fresh = collect_writers(is_session_source, &state.census.writers);
        state.apply_refresh(fresh);
        state.census.writers.contains_key(&identity)
    }
}

#[cfg(target_os = "linux")]
impl ProbeState {
    fn apply_refresh(&mut self, fresh: WriterCensus) {
        let same_visibility_scope = fresh
            .visibility_gaps
            .is_subset(&self.initial_visibility_gaps);
        // A global refresh replaces every positive, including sources that
        // were not queried. A missing PID is not proof of process exit under
        // hidepid, and a reused PID is not the previously observed owner.
        let all_owners_visible = self.census.writers.values().flatten().all(|witness| {
            let DescriptorWitness::Proc { process, .. } = witness;
            process.is_some_and(|process| fresh.visible_processes.contains(&process))
        });
        if fresh.complete && same_visibility_scope && all_owners_visible {
            self.census = fresh;
        } else {
            // Do not repeat an unreadable process-tree walk for every source,
            // and do not turn partial visibility into evidence of absence.
            self.refresh_blocked = true;
            self.census.writers.extend(fresh.writers);
        }
    }
}

#[cfg(target_os = "linux")]
impl DescriptorWitness {
    fn observe(&self, identity: FileIdentity) -> DescriptorState {
        match self {
            Self::Proc {
                descriptor, info, ..
            } => {
                match file_identity(descriptor) {
                    Ok(observed) if observed == identity => {}
                    Ok(_) => return DescriptorState::Closed,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return DescriptorState::Closed;
                    }
                    Err(_) => return DescriptorState::Uncertain,
                }
                match read_descriptor_access(info) {
                    Ok(true) => DescriptorState::Open,
                    Ok(false) => DescriptorState::Closed,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        DescriptorState::Closed
                    }
                    Err(_) => DescriptorState::Uncertain,
                }
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn file_identity(path: &Path) -> std::io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(target_os = "linux")]
fn read_descriptor_access(path: &Path) -> std::io::Result<bool> {
    let contents = std::fs::read_to_string(path)?;
    let flags = contents
        .lines()
        .find_map(|line| line.strip_prefix("flags:"))
        .and_then(|value| u64::from_str_radix(value.trim(), 8).ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unreadable procfs access flags",
            )
        })?;
    match flags & 0o3 {
        0 => Ok(false),
        1 | 2 => Ok(true),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unknown procfs access mode",
        )),
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ProcessIdentity {
    pid: u32,
    start_time: u64,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Inspection {
    DescriptorInventory,
    AccessFlags,
    Target,
    Identity,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VisibilityGap {
    process: ProcessIdentity,
    descriptor: Option<std::ffi::OsString>,
    inspection: Inspection,
    error_kind: std::io::ErrorKind,
}

#[cfg(target_os = "linux")]
fn process_identity(path: &Path, pid: u32) -> std::io::Result<ProcessIdentity> {
    let stat = std::fs::read_to_string(path.join("stat"))?;
    // comm (field 2) may itself contain spaces and ')'. Start time is field
    // 22, hence offset 19 after the final ')' that closes comm.
    let start_time = stat
        .rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|ticks| ticks.parse().ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unreadable process start time",
            )
        })?;
    Ok(ProcessIdentity { pid, start_time })
}

#[cfg(target_os = "linux")]
impl WriterCensus {
    fn observe_error(
        &mut self,
        error: std::io::Error,
        process: Option<ProcessIdentity>,
        descriptor: Option<std::ffi::OsString>,
        inspection: Inspection,
    ) {
        if error.kind() == std::io::ErrorKind::NotFound {
            return;
        }
        if let Some(process) = process {
            self.visibility_gaps.insert(VisibilityGap {
                process,
                descriptor,
                inspection,
                error_kind: error.kind(),
            });
        } else {
            // Never baseline an opaque process under a PID that can be reused.
            self.complete = false;
        }
    }
}

#[cfg(target_os = "linux")]
fn collect_writers(
    is_session_source: SourcePredicate,
    known_writers: &HashMap<FileIdentity, Vec<DescriptorWitness>>,
) -> WriterCensus {
    let mut census = WriterCensus {
        complete: true,
        ..WriterCensus::default()
    };
    let processes = match std::fs::read_dir("/proc") {
        Ok(processes) => processes,
        Err(_) => return WriterCensus::default(),
    };
    for process in processes {
        let process = match process {
            Ok(process) => process,
            Err(error) => {
                census.observe_error(error, None, None, Inspection::DescriptorInventory);
                continue;
            }
        };
        let Some(pid) = process
            .file_name()
            .to_str()
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        let process_id = process_identity(&process.path(), pid).ok();
        let descriptors = match std::fs::read_dir(process.path().join("fd")) {
            Ok(descriptors) => descriptors,
            Err(error) => {
                census.observe_error(error, process_id, None, Inspection::DescriptorInventory);
                continue;
            }
        };
        for descriptor in descriptors {
            let descriptor = match descriptor {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    census.observe_error(error, process_id, None, Inspection::DescriptorInventory);
                    continue;
                }
            };
            let info = process.path().join("fdinfo").join(descriptor.file_name());
            match read_descriptor_access(&info) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    census.observe_error(
                        error,
                        process_id,
                        Some(descriptor.file_name()),
                        Inspection::AccessFlags,
                    );
                    continue;
                }
            }
            let path = descriptor.path();
            let identity = match file_identity(&path) {
                Ok(identity) => identity,
                Err(error) => {
                    census.observe_error(
                        error,
                        process_id,
                        Some(descriptor.file_name()),
                        Inspection::Identity,
                    );
                    continue;
                }
            };
            // A known positive can have a writable successor reached through
            // a hardlink with a non-session suffix. Keep initial capture's
            // predicate scope, but match known inodes before filtering names.
            if !known_writers.contains_key(&identity) {
                let target = match std::fs::read_link(&path) {
                    Ok(target) => target,
                    Err(error) => {
                        census.observe_error(
                            error,
                            process_id,
                            Some(descriptor.file_name()),
                            Inspection::Target,
                        );
                        continue;
                    }
                };
                if !is_session_source(&target) {
                    continue;
                }
            }
            census
                .writers
                .entry(identity)
                .or_default()
                .push(DescriptorWitness::Proc {
                    descriptor: path,
                    info,
                    process: process_id,
                });
        }
        // Recheck after the descriptor inventory. A process hidden or replaced
        // during inspection cannot stand in for a still-visible old owner.
        if let Some(process_id) = process_id
            && process_identity(&process.path(), pid).ok() == Some(process_id)
        {
            census.visible_processes.insert(process_id);
        }
    }
    census
}

#[cfg(target_os = "macos")]
fn collect_writers(is_session_source: SourcePredicate) -> HashMap<FileIdentity, Vec<(u32, u32)>> {
    let Ok(output) = std::process::Command::new("lsof")
        .args(["-nP", "-F", "pfna0"])
        .output()
    else {
        return HashMap::new();
    };
    let parsed = parse_lsof(&output.stdout);
    let mut writers: HashMap<FileIdentity, Vec<(u32, u32)>> = HashMap::new();
    for record in parsed.records {
        if !record.writable || !is_session_source(&record.path) {
            continue;
        }
        if let Ok(identity) = file_identity(&record.path) {
            writers
                .entry(identity)
                .or_default()
                .push((record.process, record.descriptor));
        }
    }
    writers
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug)]
struct LsofRecord {
    process: u32,
    descriptor: u32,
    writable: bool,
    path: PathBuf,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug)]
struct ParsedLsof {
    records: Vec<LsofRecord>,
    complete: bool,
}

#[cfg(any(target_os = "macos", test))]
fn parse_lsof(output: &[u8]) -> ParsedLsof {
    // lsof's field-output contract assigns access mode to `a`; `f` starts
    // a descriptor record. NUL terminators keep embedded newlines in names
    // from becoming forged process/descriptor fields.
    let mut parsed = ParsedLsof {
        records: Vec::new(),
        complete: output.ends_with(b"\0") || output.ends_with(b"\0\n"),
    };
    let mut process = None;
    let mut descriptor = None;
    let mut writable = None;
    let mut path = None;
    let finish_record = |parsed: &mut ParsedLsof,
                         process: Option<u32>,
                         descriptor: Option<u32>,
                         writable: Option<bool>,
                         path: Option<PathBuf>| {
        if let Some(descriptor) = descriptor {
            if let (Some(process), Some(writable), Some(path)) = (process, writable, path) {
                parsed.records.push(LsofRecord {
                    process,
                    descriptor,
                    writable,
                    path,
                });
            } else {
                parsed.complete = false;
            }
        }
    };
    for field in output.split(|byte| *byte == 0) {
        let field = field.strip_prefix(b"\n").unwrap_or(field);
        let Some((&kind, value)) = field.split_first() else {
            continue;
        };
        let Ok(value) = std::str::from_utf8(value) else {
            parsed.complete = false;
            continue;
        };
        match kind {
            b'p' => {
                finish_record(&mut parsed, process, descriptor, writable, path.take());
                process = value.parse::<u32>().ok();
                parsed.complete &= process.is_some();
                descriptor = None;
                writable = None;
            }
            b'f' => {
                finish_record(&mut parsed, process, descriptor, writable, path.take());
                descriptor = value.parse::<u32>().ok();
                writable = None;
                // Named pseudo descriptors are not open numeric descriptors.
                // Unknown/error values (including a human-format `3w`) cannot
                // certify a complete field-output census.
                if descriptor.is_none()
                    && !matches!(
                        value,
                        "cwd" | "rtd" | "txt" | "mem" | "mmap" | "ltx" | "pd" | "jld" | "v86"
                    )
                {
                    parsed.complete = false;
                }
            }
            b'a' if descriptor.is_some() => {
                if writable.is_some() {
                    parsed.complete = false;
                }
                writable = match value {
                    "r" => Some(false),
                    "w" | "u" => Some(true),
                    _ => None,
                };
            }
            b'n' if descriptor.is_some() => {
                if path.is_some() || value.is_empty() {
                    parsed.complete = false;
                }
                path = Some(PathBuf::from(value));
            }
            b'a' | b'n' => {}
            _ => parsed.complete = false,
        }
    }
    finish_record(&mut parsed, process, descriptor, writable, path);
    parsed.complete &= process.is_some();
    parsed
}

#[cfg(test)]
#[path = "source_writer_probe/tests.rs"]
mod tests;
