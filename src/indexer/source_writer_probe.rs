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
//! procfs reads. The census reads each writable descriptor's link without
//! following it. Network and FUSE file identities come from fdinfo and
//! mountinfo, without contacting the mount's server or userspace daemon.
//! Missing procfs identity fields leave an inspection gap, never a fallback
//! stat on a potentially hung mount. Only proven local descriptors are
//! followed, including non-session hardlinks while refreshing a positive.
//! One refresh replaces evidence for every source, amortizing closed writers.
//! A newly incomplete refresh retains all prior positives for the filter's life.
//! macOS lsof can silently omit inaccessible processes; its captured positives
//! therefore remain deferred for the entire filter lifetime without reinspection.
//! Its device/inode fields supply identities without statting descriptor paths.
//! A bounded, nonblocking census keeps a hung mount or inherited output stream
//! from stalling indexing; incomplete output still contributes observed positives.

#[cfg(any(target_os = "linux", target_os = "macos", test))]
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
        let census = collect_writers(is_session_source);
        if census.visibility_gap {
            tracing::debug!(
                observed_writers = census.writers.len(),
                "source writer census has visibility gaps; retaining observed positives"
            );
        }
        Self {
            writers: census.writers,
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
    /// Why the last refresh kept the prior evidence. Live tests use it to
    /// tell an unrelated process turning opaque mid-census (routine for a
    /// non-root user on a busy host) from a probe defect (z6ycn).
    #[cfg(test)]
    last_refresh_block: Option<RefreshBlock>,
}

#[cfg(all(test, target_os = "linux"))]
#[derive(Debug)]
struct RefreshBlock {
    complete: bool,
    owners_visible: bool,
    new_gap_pids: Vec<u32>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct WriterCensus {
    writers: HashMap<FileIdentity, Vec<DescriptorWitness>>,
    visibility_gaps: HashSet<VisibilityGap>,
    visible_processes: HashSet<ProcessIdentity>,
    complete: bool,
    /// Descriptor targets this census followed to their file's filesystem.
    #[cfg(test)]
    followed: Vec<PathBuf>,
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
            #[cfg(test)]
            {
                self.last_refresh_block = Some(RefreshBlock {
                    complete: fresh.complete,
                    owners_visible: all_owners_visible,
                    new_gap_pids: fresh
                        .visibility_gaps
                        .difference(&self.initial_visibility_gaps)
                        .map(|gap| gap.process.pid)
                        .collect(),
                });
            }
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
                let observed = match read_descriptor_info(info) {
                    Ok(observed) if observed.writable => observed,
                    Ok(_) => return DescriptorState::Closed,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return DescriptorState::Closed;
                    }
                    Err(_) => return DescriptorState::Uncertain,
                };
                let Some(process_path) = info.parent().and_then(Path::parent) else {
                    return DescriptorState::Uncertain;
                };
                // A descriptor number may have been reused or its owner may
                // have changed mount namespaces since capture. Re-read procfs
                // authority instead of following a formerly local descriptor.
                let mount = descriptor_mount(process_path, &observed, &mut None);
                let current = match descriptor_identity(descriptor, &observed, mount) {
                    Ok(current) => current,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return DescriptorState::Closed;
                    }
                    Err(_) => return DescriptorState::Uncertain,
                };
                // Never combine an old writer's access flags with a reused
                // descriptor's identity. Changed evidence cannot clear a
                // cached positive until a complete census finds its successor.
                if read_descriptor_info(info).ok() != Some(observed) {
                    return DescriptorState::Uncertain;
                }
                if current == identity {
                    DescriptorState::Open
                } else {
                    DescriptorState::Closed
                }
            }
        }
    }
}

#[cfg(any(target_os = "linux", all(target_os = "macos", test)))]
fn file_identity(path: &Path) -> std::io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DescriptorInfo {
    writable: bool,
    mount_id: Option<u64>,
    inode: Option<u64>,
}

#[cfg(target_os = "linux")]
fn read_descriptor_info(path: &Path) -> std::io::Result<DescriptorInfo> {
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
    let writable = match flags & 0o3 {
        0 => false,
        1 | 2 => true,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown procfs access mode",
            ));
        }
    };
    let decimal_field = |prefix: &str| {
        contents
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .and_then(|value| value.trim().parse::<u64>().ok())
    };
    Ok(DescriptorInfo {
        writable,
        mount_id: decimal_field("mnt_id:"),
        inode: decimal_field("ino:"),
    })
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MountIdentity {
    device: u64,
    remote: bool,
}

/// Resolve the descriptor's mount in its owner's namespace, using procfs
/// alone. A missing or malformed record never establishes local access.
#[cfg(target_os = "linux")]
fn descriptor_mount(
    process: &Path,
    info: &DescriptorInfo,
    mounts: &mut Option<HashMap<u64, MountIdentity>>,
) -> Option<MountIdentity> {
    let mount_id = info.mount_id?;
    mounts
        .get_or_insert_with(|| {
            std::fs::read_to_string(process.join("mountinfo"))
                .map(|table| parse_mount_identities(&table))
                .unwrap_or_default()
        })
        .get(&mount_id)
        .copied()
}

/// Only local mounts permit metadata I/O. Since Linux 5.14, regular-file
/// fdinfo supplies `ino`; mountinfo supplies the major/minor of `st_dev`.
/// Older kernels or an opaque namespace cannot supply a remote identity,
/// so preserve the gap instead of potentially blocking the whole index run.
#[cfg(target_os = "linux")]
fn descriptor_identity(
    descriptor: &Path,
    info: &DescriptorInfo,
    mount: Option<MountIdentity>,
) -> std::io::Result<FileIdentity> {
    match mount {
        Some(mount) if !mount.remote => file_identity(descriptor),
        Some(mount) => info
            .inode
            .map(|inode| (mount.device, inode))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "remote session descriptor has no procfs inode",
                )
            }),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "session descriptor mount identity is unverified",
        )),
    }
}

/// Linux's userspace dev_t encoding, also used by makedev(3). Mountinfo
/// prints decimal major/minor components, rather than this packed value.
#[cfg(target_os = "linux")]
fn linux_device_id(major: u32, minor: u32) -> u64 {
    let major = u64::from(major);
    let minor = u64::from(minor);
    (minor & 0xff) | ((major & 0xfff) << 8) | ((minor & !0xff) << 12) | ((major & !0xfff) << 32)
}

/// Map mount IDs to their device and filesystem type. Mount paths escape
/// spaces, so the first " - " always ends the optional fields.
#[cfg(target_os = "linux")]
fn parse_mount_identities(table: &str) -> HashMap<u64, MountIdentity> {
    table
        .lines()
        .filter_map(|line| {
            let (before_separator, after_separator) = line.split_once(" - ")?;
            let mut fields = before_separator.split_whitespace();
            let mount_id = fields.next()?.parse::<u64>().ok()?;
            let (major, minor) = fields.nth(1)?.split_once(':')?;
            let device = linux_device_id(major.parse().ok()?, minor.parse().ok()?);
            let filesystem_type = after_separator.split_whitespace().next()?;
            Some((
                mount_id,
                MountIdentity {
                    device,
                    remote: filesystem_type_is_remote(filesystem_type),
                },
            ))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn filesystem_type_is_remote(filesystem_type: &str) -> bool {
    // fuse, fuseblk and every fuse.<subtype> are answered by a userspace
    // daemon; the rest by a server over the network. fusectl, FUSE's own
    // control filesystem, is answered by the kernel.
    filesystem_type.starts_with("fuse.")
        || matches!(
            filesystem_type,
            "fuse"
                | "fuseblk"
                | "nfs"
                | "nfs4"
                | "cifs"
                | "smb3"
                | "smbfs"
                | "ceph"
                | "9p"
                | "afs"
                | "virtiofs"
                | "lustre"
                | "gpfs"
                | "glusterfs"
                | "beegfs"
                | "orangefs"
                | "pvfs2"
                | "ncpfs"
                | "coda"
        )
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
    collect_writers_in(Path::new("/proc"), is_session_source, known_writers)
}

#[cfg(target_os = "linux")]
fn collect_writers_in(
    proc_root: &Path,
    is_session_source: SourcePredicate,
    known_writers: &HashMap<FileIdentity, Vec<DescriptorWitness>>,
) -> WriterCensus {
    let mut census = WriterCensus {
        complete: true,
        ..WriterCensus::default()
    };
    let processes = match std::fs::read_dir(proc_root) {
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
        // This process's mount table, read at most once and only if a
        // descriptor needs it.
        let mut mounts = None;
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
            let observed = match read_descriptor_info(&info) {
                Ok(observed) if observed.writable => observed,
                Ok(_) => continue,
                Err(error) => {
                    census.observe_error(
                        error,
                        process_id,
                        Some(descriptor.file_name()),
                        Inspection::AccessFlags,
                    );
                    continue;
                }
            };
            let path = descriptor.path();
            // Read the link before following it. readlink on a procfs
            // descriptor is answered by the kernel, while a stat that follows
            // it is answered by the file's own filesystem: a process holding
            // a file on an unresponsive network or FUSE mount would otherwise
            // block this census, and the whole index run, in the kernel.
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
            let session_named = is_session_source(&target);
            if !session_named && known_writers.is_empty() {
                continue;
            }
            let mount = descriptor_mount(&process.path(), &observed, &mut mounts);
            // A known positive can have a writable successor reached through
            // a hardlink with a non-session suffix, so while positives are
            // cached a non-session descriptor is followed and matched against
            // their inodes. Only a local one: a session source on a network
            // or FUSE mount is still found under its session name, but not
            // through such a hardlink.
            if !session_named && mount.is_none_or(|mount| mount.remote) {
                continue;
            }
            #[cfg(test)]
            if mount.is_some_and(|mount| !mount.remote) {
                census.followed.push(target);
            }
            let identity = match descriptor_identity(&path, &observed, mount) {
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
            if !session_named && !known_writers.contains_key(&identity) {
                continue;
            }
            let unchanged = match read_descriptor_info(&info) {
                Ok(after) if after == observed => Ok(()),
                Ok(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "descriptor changed during writer census",
                )),
                Err(error) => Err(error),
            };
            if let Err(error) = unchanged {
                census.observe_error(
                    error,
                    process_id,
                    Some(descriptor.file_name()),
                    Inspection::Identity,
                );
                continue;
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
fn collect_writers(is_session_source: SourcePredicate) -> LsofWriterCensus {
    // One retained child bounds both concurrent censuses and an OS-stalled
    // teardown. Later calls only try to reap it; they never wait or spawn more.
    static CHILD: std::sync::Mutex<Option<std::process::Child>> = std::sync::Mutex::new(None);
    match capture_lsof_output(
        &mut lsof_command(),
        &CHILD,
        std::time::Duration::from_secs(5),
        16 * 1024 * 1024,
    ) {
        Ok(output) => lsof_writer_census(&output.stdout, output.complete, is_session_source),
        Err(error) => {
            tracing::debug!(%error, "source writer census could not complete");
            LsofWriterCensus {
                visibility_gap: true,
                ..LsofWriterCensus::default()
            }
        }
    }
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Default)]
struct LsofWriterCensus {
    writers: HashMap<FileIdentity, Vec<(u32, u32)>>,
    visibility_gap: bool,
}

#[cfg(any(target_os = "macos", test))]
fn lsof_writer_census(
    output: &[u8],
    complete: bool,
    is_session_source: SourcePredicate,
) -> LsofWriterCensus {
    let parsed = parse_lsof(output);
    let mut census = LsofWriterCensus {
        visibility_gap: !complete || !parsed.complete,
        ..LsofWriterCensus::default()
    };
    for record in parsed.records {
        if !record.writable || !is_session_source(&record.path) {
            continue;
        }
        if let Some(identity) = record.identity {
            census
                .writers
                .entry(identity)
                .or_default()
                .push((record.process, record.descriptor));
        } else {
            // A missing identity is an inspection gap, never permission to
            // stat a potentially unresponsive remote path as a fallback.
            census.visibility_gap = true;
        }
    }
    census
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn lsof_command() -> std::process::Command {
    let mut command = std::process::Command::new("/usr/sbin/lsof");
    // -b avoids lsof's own stat/lstat/readlink calls. D is the packed device
    // in hex, i the decimal inode; neither requires following the n field.
    command.args(["-b", "-nP", "-F", "pfnaDi0"]);
    command
}

#[cfg(any(target_os = "macos", all(test, unix)))]
#[derive(Debug)]
struct LsofOutput {
    stdout: Vec<u8>,
    complete: bool,
}

/// Collect the available prefix without a blocking read, wait, or reader thread.
/// The slot also owns children that cannot be reaped within the cleanup grace.
/// Those are retried on the next census, limiting retained direct children to one.
/// Process creation itself is synchronous; the deadline cannot preempt spawn.
#[cfg(any(target_os = "macos", all(test, unix)))]
fn capture_lsof_output(
    command: &mut std::process::Command,
    slot: &std::sync::Mutex<Option<std::process::Child>>,
    timeout: std::time::Duration,
    max_bytes: usize,
) -> std::io::Result<LsofOutput> {
    use std::io::{self, Read};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let mut slot = slot
        .try_lock()
        .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "lsof census already active"))?;
    if let Some(child) = slot.as_mut() {
        if child.try_wait()?.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "previous lsof census is still exiting",
            ));
        }
        *slot = None;
    }
    if timeout.is_zero() {
        return Ok(LsofOutput {
            stdout: Vec::new(),
            complete: false,
        });
    }

    let (mut reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::null())
        .process_group(0);
    let started = Instant::now();
    let spawned = command.spawn();
    // Command retains its stdout descriptor after spawn. Close that copy so
    // EOF depends only on the child and any descendants that inherited it.
    command.stdout(Stdio::null());
    *slot = Some(spawned?);

    let mut output = LsofOutput {
        stdout: Vec::new(),
        complete: false,
    };
    let mut eof = false;
    let mut buffer = [0_u8; 8 * 1024];
    'capture: loop {
        if started.elapsed() >= timeout {
            break;
        }
        let mut progressed = false;
        // A continuously writable stream cannot starve the deadline check.
        for _ in 0..4 {
            if eof || started.elapsed() >= timeout {
                break;
            }
            let remaining = max_bytes.saturating_sub(output.stdout.len());
            let read_len = remaining.saturating_add(1).min(buffer.len());
            match reader.read(&mut buffer[..read_len]) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(count) => {
                    let retained = count.min(remaining);
                    let total = output.stdout.len() + retained;
                    if total > output.stdout.capacity() {
                        let capacity = output
                            .stdout
                            .capacity()
                            .saturating_mul(2)
                            .max(total)
                            .min(max_bytes);
                        if output
                            .stdout
                            .try_reserve_exact(capacity - output.stdout.len())
                            .is_err()
                        {
                            break 'capture;
                        }
                    }
                    output.stdout.extend_from_slice(&buffer[..retained]);
                    if count > remaining {
                        break 'capture;
                    }
                    progressed = true;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break 'capture,
            }
        }
        // Keep the direct child unreaped while descendants could retain the
        // socket: its PID must not be recycled before failure group cleanup.
        if eof && let Some(child) = slot.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    output.complete = status.success();
                    *slot = None;
                    return Ok(output);
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
        if !progressed {
            std::thread::sleep(
                timeout
                    .saturating_sub(started.elapsed())
                    .min(Duration::from_millis(5)),
            );
        }
    }
    drop(reader);
    stop_lsof_child(&mut slot, Duration::from_millis(100));
    Ok(output)
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn stop_lsof_child(child: &mut Option<std::process::Child>, grace: std::time::Duration) {
    let Some(owned) = child.as_mut() else {
        return;
    };
    kill_lsof_process_group(owned.id());
    let _ = owned.kill();
    let started = std::time::Instant::now();
    loop {
        match owned.try_wait() {
            Ok(Some(_)) => {
                *child = None;
                return;
            }
            Ok(None) if started.elapsed() < grace => std::thread::sleep(
                grace
                    .saturating_sub(started.elapsed())
                    .min(std::time::Duration::from_millis(5)),
            ),
            _ => return,
        }
    }
}

#[cfg(any(target_os = "macos", all(test, unix)))]
#[allow(unsafe_code)]
fn kill_lsof_process_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid)
        && pid > 1
    {
        // SAFETY: capture_lsof_output creates a new group for this child and
        // keeps it unreaped until here. Its PID cannot be reused for another
        // process group. kill takes an integer ID, no pointers or ownership.
        let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug)]
struct LsofRecord {
    process: u32,
    descriptor: u32,
    writable: bool,
    path: PathBuf,
    identity: Option<FileIdentity>,
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
    let mut identity_fields = (None, None);
    let mut record_valid = true;
    let finish_record = |parsed: &mut ParsedLsof,
                         process: Option<u32>,
                         descriptor: Option<u32>,
                         writable: Option<bool>,
                         path: Option<PathBuf>,
                         identity_fields: (Option<u64>, Option<u64>),
                         record_valid: bool| {
        if let Some(descriptor) = descriptor {
            if let (Some(process), Some(writable), Some(path)) = (process, writable, path)
                && record_valid
            {
                parsed.records.push(LsofRecord {
                    process,
                    descriptor,
                    writable,
                    path,
                    identity: identity_fields.0.zip(identity_fields.1),
                });
            } else {
                parsed.complete = false;
            }
        }
    };
    for field in output.split_inclusive(|byte| *byte == 0) {
        let Some(field) = field.strip_suffix(b"\0") else {
            // Only NUL-terminated fields are evidence. A final NL is the
            // documented set terminator; any other suffix is truncated.
            parsed.complete &= field == b"\n";
            break;
        };
        let field = field.strip_prefix(b"\n").unwrap_or(field);
        let Some((&kind, value)) = field.split_first() else {
            continue;
        };
        // Boundaries apply even if the next PID/descriptor is malformed or
        // not UTF-8. Never attach its following fields to the previous owner.
        if matches!(kind, b'p' | b'f') {
            finish_record(
                &mut parsed,
                process,
                descriptor,
                writable,
                path.take(),
                identity_fields,
                record_valid,
            );
            descriptor = None;
            writable = None;
            identity_fields = (None, None);
            record_valid = true;
            if kind == b'p' {
                process = None;
            }
        }
        let Ok(value) = std::str::from_utf8(value) else {
            parsed.complete = false;
            record_valid = false;
            continue;
        };
        match kind {
            b'p' => {
                process = value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit())
                    .then(|| value.parse::<u32>().ok())
                    .flatten()
                    .filter(|pid| *pid > 0);
                parsed.complete &= process.is_some();
            }
            b'f' => {
                descriptor = value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit())
                    .then(|| value.parse::<u32>().ok())
                    .flatten();
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
                    record_valid = false;
                }
                writable = match value {
                    "r" => Some(false),
                    "w" | "u" => Some(true),
                    _ => None,
                };
                record_valid &= writable.is_some();
            }
            b'n' if descriptor.is_some() => {
                if path.is_some() || value.is_empty() {
                    parsed.complete = false;
                    record_valid = false;
                }
                path = Some(PathBuf::from(value));
            }
            b'D' if descriptor.is_some() => {
                if identity_fields.0.is_some() {
                    record_valid = false;
                }
                identity_fields.0 = parse_lsof_device(value);
                record_valid &= identity_fields.0.is_some();
            }
            b'i' if descriptor.is_some() => {
                if identity_fields.1.is_some() {
                    record_valid = false;
                }
                identity_fields.1 = if value.bytes().all(|byte| byte.is_ascii_digit()) {
                    value.parse::<u64>().ok()
                } else {
                    None
                };
                record_valid &= identity_fields.1.is_some();
            }
            b'a' | b'n' | b'D' | b'i' => {}
            _ => parsed.complete = false,
        }
    }
    finish_record(
        &mut parsed,
        process,
        descriptor,
        writable,
        path,
        identity_fields,
        record_valid,
    );
    parsed.complete &= process.is_some();
    parsed
}

#[cfg(any(target_os = "macos", test))]
fn parse_lsof_device(value: &str) -> Option<u64> {
    let digits = value.strip_prefix("0x")?;
    if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    // Apple's lsof/proc.c prints dev_t after casting to unsigned int. Rust's
    // Darwin MetadataExt::st_dev casts the signed 32-bit dev_t directly to
    // u64. Restore that sign extension so high-bit devices match the caller.
    Some((u32::from_str_radix(digits, 16).ok()? as i32) as u64)
}

#[cfg(test)]
#[path = "source_writer_probe/tests.rs"]
mod tests;
