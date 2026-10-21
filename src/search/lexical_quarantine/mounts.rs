//! Linux mount boundaries are distinct from filesystem-device boundaries.
//! A bind mount can expose unrelated files with the same st_dev and nlink=1.
//! Check the current thread's mount namespace before walking a bundle and on
//! both sides of the retention rename. This is not an atomic mount-operation
//! exclusion: operators must not change mounts during inspection/reclamation.

use super::Budget;
use std::io;
use std::path::Path;

pub(super) fn check(path: &Path, budget: &mut Budget) -> io::Result<()> {
    budget.check()?;
    #[cfg(target_os = "linux")]
    {
        // Do not substitute /proc/mounts or the leader's namespace: mountinfo
        // names bind mounts too, and a thread can have a different namespace.
        let canonical = std::fs::canonicalize(path)?;
        let input = std::fs::File::open("/proc/thread-self/mountinfo").map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot verify quarantine mount boundary: {error}"),
            )
        })?;
        check_reader(input, &canonical, budget)?;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = path;
    budget.check()
}

#[cfg(target_os = "linux")]
const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_WORK_BYTES: usize = 16 * 1024 * 1024;

#[cfg(target_os = "linux")]
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Bounded, byte-preserving parsing; no mount-point or payload file is opened.
/// Shared with fault-injected tests, never configurable through CLI/environment.
#[cfg(target_os = "linux")]
pub(super) fn check_reader(
    input: impl io::Read,
    canonical: &Path,
    budget: &mut Budget,
) -> io::Result<()> {
    use std::io::Read;

    budget.check()?;
    let remaining = MAX_WORK_BYTES.saturating_sub(budget.mount_bytes);
    if remaining == 0 {
        return Err(invalid("quarantine mount-table work budget exhausted"));
    }
    let limit = remaining.min(MAX_SNAPSHOT_BYTES);
    let mut bytes = Vec::new();
    let result = input.take((limit + 1) as u64).read_to_end(&mut bytes);
    // Charge even partial/error reads. One sentinel byte detects truncation;
    // a failed read cannot grant a fresh allowance to the next bundle.
    budget.mount_bytes = budget.mount_bytes.saturating_add(bytes.len());
    result?;
    budget.check()?;
    if bytes.len() > limit {
        return Err(invalid("quarantine mount-table byte limit exceeded"));
    }
    if bytes.is_empty() || !bytes.ends_with(b"\n") {
        return Err(invalid("quarantine mount table is empty or truncated"));
    }
    if !canonical.is_absolute() {
        return Err(invalid("quarantine mount check requires an absolute path"));
    }
    for line in bytes[..bytes.len() - 1].split(|&byte| byte == b'\n') {
        budget.check()?;
        let mountpoint = parse_mountpoint(line)?;
        if mountpoint.starts_with(canonical) {
            return Err(io::Error::other(format!(
                "quarantine contains mount point {mountpoint:?}; retain for inspection"
            )));
        }
    }
    budget.check()
}

#[cfg(target_os = "linux")]
fn parse_mountpoint(line: &[u8]) -> io::Result<std::path::PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::Component;

    let bad = || invalid("malformed quarantine mount table; retention refused");
    let decimal =
        |value: &[u8]| !value.is_empty() && value.iter().all(|byte| byte.is_ascii_digit());
    let mut fields = line
        .split(|&byte| byte == b' ')
        .filter(|field| !field.is_empty());
    if !decimal(fields.next().ok_or_else(bad)?) || !decimal(fields.next().ok_or_else(bad)?) {
        return Err(bad());
    }
    let mut device = fields.next().ok_or_else(bad)?.split(|&byte| byte == b':');
    if !decimal(device.next().ok_or_else(bad)?)
        || !decimal(device.next().ok_or_else(bad)?)
        || device.next().is_some()
    {
        return Err(bad());
    }
    if !fields.next().ok_or_else(bad)?.starts_with(b"/") {
        return Err(bad());
    }
    let encoded = fields.next().ok_or_else(bad)?;
    fields.next().ok_or_else(bad)?; // Per-mount options.
    // The kernel contract requires ignoring unrecognized optional fields.
    if !fields.by_ref().any(|field| field == b"-") {
        return Err(bad());
    }
    for _ in 0..3 {
        fields.next().ok_or_else(bad)?; // Filesystem, source, superblock options.
    }
    if fields.next().is_some() || line.contains(&0) {
        return Err(bad());
    }
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut offset = 0;
    while offset < encoded.len() {
        if encoded[offset] == b'\\' {
            let escape = encoded.get(offset..offset + 4).ok_or_else(bad)?;
            decoded.push(match escape {
                b"\\040" => b' ',
                b"\\011" => b'\t',
                b"\\012" => b'\n',
                b"\\134" => b'\\',
                _ => return Err(bad()),
            });
            offset += 4;
        } else {
            decoded.push(encoded[offset]);
            offset += 1;
        }
    }
    let path = std::path::PathBuf::from(OsString::from_vec(decoded));
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(bad());
    }
    Ok(path)
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
