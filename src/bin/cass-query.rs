//! Fresh native frontend for the explicit guarded lexical IPC owner.
//! This binary neither opens indexes nor silently starts a background worker.
#![deny(unsafe_code)]

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{File, OpenOptions};
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
    use std::os::unix::net::UnixStream;
    use std::path::{Component, Path, PathBuf};
    use std::sync::mpsc;
    use std::thread::JoinHandle;
    use std::time::Duration;

    use anyhow::{Context, Result, bail, ensure};
    use clap::Parser;
    use serde::Deserialize;
    use serde_json::{Value, json};

    const MAX_REQUEST: usize = 64 * 1024;
    const MAX_RESPONSE: usize = 1024 * 1024;

    #[derive(Debug, Parser)]
    #[command(
        name = "cass-query",
        version,
        about = "Fresh lexical client for an explicitly managed local CASS IPC owner"
    )]
    pub struct Args {
        /// Private same-user Unix socket created by scripts/cass_lexical_ipc.py.
        #[arg(long)]
        socket: PathBuf,
        /// Lexical query; results use the cass serve JSON envelope, not cass search formatting.
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: u64,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        /// JSON object containing the ordinary cass serve lexical filters.
        #[arg(long)]
        filters: Option<String>,
        /// Destroy the retained native reader and fully verify a new reader.
        #[arg(long)]
        full_verify: bool,
        /// Whole-operation deadline, including blocked output; expiry exits 124.
        #[arg(long, default_value_t = 30_000, value_parser =
            clap::value_parser!(u64).range(1..=300_000))]
        timeout_ms: u64,
    }

    /// Bound blocking filesystem operations/connect/output as well as socket I/O.
    /// No detached timeout thread survives a successful or failed request.
    struct Deadline {
        cancel: mpsc::Sender<()>,
        handle: Option<JoinHandle<()>>,
    }

    impl Deadline {
        fn start(timeout: Duration) -> Result<Self> {
            let (cancel, receiver) = mpsc::channel();
            let handle = std::thread::Builder::new()
                .name("cass-query-deadline".into())
                .spawn(move || {
                    if matches!(
                        receiver.recv_timeout(timeout),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        // Do not print first: stderr could itself be blocked.
                        std::process::exit(124);
                    }
                })
                .context("start request deadline")?;
            Ok(Self {
                cancel,
                handle: Some(handle),
            })
        }
    }

    impl Drop for Deadline {
        fn drop(&mut self) {
            let _ = self.cancel.send(());
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[allow(unsafe_code)]
    fn effective_uid() -> libc::uid_t {
        // SAFETY: geteuid takes no pointers and has no preconditions.
        unsafe { libc::geteuid() }
    }

    /// Keep the parent descriptor alive through connect. Walk each component
    /// without following symlinks; a renamed ancestor cannot redirect the socket.
    fn pinned_endpoint(path: &Path) -> Result<(File, PathBuf)> {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        let name = absolute.file_name().context("socket filename is missing")?;
        let parent = absolute.parent().context("socket parent is missing")?;
        let mut directory = File::open("/")?;
        for component in parent.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(part) => {
                    let next = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
                        .join(part);
                    directory = OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                        .open(next)
                        .context("open socket directory without symlinks")?;
                }
                _ => bail!("socket path must not contain parent traversal"),
            }
        }
        let metadata = directory.metadata()?;
        ensure!(
            metadata.uid() == effective_uid() && metadata.mode() & 0o077 == 0,
            "socket parent must be owned by you and mode 0700"
        );
        let endpoint = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name);
        let metadata = std::fs::symlink_metadata(&endpoint)?;
        ensure!(
            metadata.file_type().is_socket()
                && metadata.uid() == effective_uid()
                && metadata.mode() & 0o077 == 0,
            "unsafe lexical socket"
        );
        Ok((directory, endpoint))
    }

    #[allow(unsafe_code)]
    fn authenticate_peer(stream: &UnixStream) -> Result<()> {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: stream owns a live descriptor. The writable credential buffer
        // and length pointer have exactly the types/sizes expected by SO_PEERCRED.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut credentials).cast(),
                &raw mut size,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("authenticate socket peer");
        }
        ensure!(
            size as usize == std::mem::size_of::<libc::ucred>()
                && credentials.uid == effective_uid(),
            "socket peer is not the same user"
        );
        Ok(())
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Envelope {
        schema_version: u64,
        id: u64,
        ok: bool,
        result: Option<Value>,
        error: Option<Value>,
        admission: Option<Admission>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Admission {
        mode: String,
        owner_epoch: u64,
        full_verify_requested: bool,
        persistent_proof: bool,
        file_identity_checked: bool,
        immutable_generation_certified: bool,
    }

    fn validate_response(bytes: &[u8], full_verify: bool) -> Result<bool> {
        let response: Envelope = serde_json::from_slice(bytes).context("decode owner response")?;
        ensure!(
            response.schema_version == 1 && response.id == 1,
            "mismatched response envelope"
        );
        if !response.ok {
            ensure!(
                response.result.is_none()
                    && response.admission.is_none()
                    && response.error.as_ref().is_some_and(Value::is_object),
                "malformed error response"
            );
            return Ok(false);
        }
        ensure!(
            response.error.is_none(),
            "successful response also contains an error"
        );
        let result = response
            .result
            .filter(Value::is_object)
            .context("missing result")?;
        let admission = response.admission.context("missing admission policy")?;
        ensure!(
            admission.owner_epoch > 0
                && !admission.persistent_proof
                && admission.file_identity_checked
                && !admission.immutable_generation_certified
                && admission.full_verify_requested == full_verify,
            "unexpected admission contract"
        );
        let reused = match admission.mode.as_str() {
            "strict_full" => false,
            "retained_guarded" if !full_verify => true,
            _ => bail!("owner did not honor the requested verification policy"),
        };
        ensure!(
            result.get("reader_reused").and_then(Value::as_bool) == Some(reused),
            "native reader lifecycle does not match the admission policy"
        );
        Ok(true)
    }

    fn request(args: &Args) -> Result<Vec<u8>> {
        ensure!(
            !args.query.trim().is_empty() && args.query.len() <= 4096,
            "query must contain 1 to 4096 UTF-8 bytes"
        );
        ensure!(
            (1..=100).contains(&args.limit)
                && args
                    .offset
                    .checked_add(args.limit)
                    .and_then(|n| n.checked_add(1))
                    .is_some_and(|n| n <= 1024),
            "invalid pagination budget"
        );
        let filters = match args.filters.as_deref() {
            Some(text) => serde_json::from_str::<Value>(text).context("decode --filters")?,
            None => json!({}),
        };
        ensure!(filters.is_object(), "--filters must be a JSON object");
        let mut bytes = serde_json::to_vec(&json!({
            "op": "search", "id": 1, "query": args.query.as_str(),
            "limit": args.limit, "offset": args.offset, "filters": filters,
            "full_verify": args.full_verify,
        }))?;
        bytes.push(b'\n');
        ensure!(bytes.len() <= MAX_REQUEST, "request exceeds 64 KiB");
        Ok(bytes)
    }

    pub fn run() -> Result<bool> {
        let args = Args::parse();
        let timeout = Duration::from_millis(args.timeout_ms);
        let _deadline = Deadline::start(timeout)?;
        let outcome = (|| -> Result<bool> {
            let request = request(&args)?;
            let (_directory, endpoint) = pinned_endpoint(&args.socket)?;
            let mut stream = UnixStream::connect(endpoint).context("connect lexical owner")?;
            authenticate_peer(&stream)?;
            stream.set_read_timeout(Some(timeout))?;
            stream.set_write_timeout(Some(timeout))?;
            stream.write_all(&request).context("send lexical request")?;

            // read_until alone is unbounded. fill_buf/consume enforces the cap
            // before appending bytes, including when a peer never sends a newline.
            let mut reader = BufReader::new(stream);
            let mut response = Vec::new();
            loop {
                let chunk = reader.fill_buf().context("read lexical response")?;
                ensure!(!chunk.is_empty(), "truncated lexical response");
                let newline = chunk.iter().position(|byte| *byte == b'\n');
                let count = newline.map_or(chunk.len(), |position| position + 1);
                ensure!(
                    response.len() + count <= MAX_RESPONSE,
                    "response exceeds 1 MiB"
                );
                response.extend_from_slice(&chunk[..count]);
                reader.consume(count);
                if newline.is_some() {
                    ensure!(reader.buffer().is_empty(), "unexpected trailing response");
                    break;
                }
            }
            let ok = validate_response(&response, args.full_verify)?;
            let mut output = std::io::stdout().lock();
            output.write_all(&response)?;
            output.flush()?;
            Ok(ok)
        })();
        if let Err(error) = &outcome {
            eprintln!(
                "{}",
                json!({
                    "ok": false, "error": {"kind": "lexical_ipc", "message": error.to_string()}
                })
            );
        }
        outcome
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn response(mode: &str, full: bool, reused: bool) -> Value {
            json!({
                "schema_version": 1, "id": 1, "ok": true,
                "result": {"hits": [], "reader_reused": reused},
                "admission": {
                    "mode": mode, "owner_epoch": 1, "full_verify_requested": full,
                    "persistent_proof": false, "file_identity_checked": true,
                    "immutable_generation_certified": false,
                }
            })
        }

        #[test]
        fn honors_full_verification_and_lifecycle() {
            for (mode, full, reused) in [
                ("strict_full", false, false),
                ("strict_full", true, false),
                ("retained_guarded", false, true),
            ] {
                assert!(
                    validate_response(
                        &serde_json::to_vec(&response(mode, full, reused)).unwrap(),
                        full
                    )
                    .unwrap()
                );
            }
            for (mode, full, reused) in [
                ("retained_guarded", true, true),
                ("strict_full", true, true),
                ("unchecked", false, true),
            ] {
                assert!(
                    validate_response(
                        &serde_json::to_vec(&response(mode, full, reused)).unwrap(),
                        full
                    )
                    .is_err()
                );
            }
        }

        #[test]
        fn rejects_corrupted_admission_and_duplicate_envelopes() {
            let mut bad = response("strict_full", false, false);
            bad["admission"]["persistent_proof"] = json!(true);
            assert!(validate_response(&serde_json::to_vec(&bad).unwrap(), false).is_err());
            assert!(
                validate_response(
                    br#"{"schema_version":1,"id":1,"ok":true,"ok":false}"#,
                    false
                )
                .is_err()
            );
            assert!(
                validate_response(
                    br#"{"schema_version":1,"id":2,"ok":false,
                "error":{"kind":"failed"}}"#,
                    false
                )
                .is_err()
            );
        }

        #[test]
        fn refuses_insecure_or_symlink_parents() {
            use std::os::unix::fs::{PermissionsExt, symlink};
            use std::os::unix::net::UnixListener;
            let dir = tempfile::tempdir().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let socket = dir.path().join("worker.sock");
            let _listener = UnixListener::bind(&socket).unwrap();
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(pinned_endpoint(&socket).is_ok());
            let link = dir.path().join("linked");
            symlink(dir.path(), &link).unwrap();
            assert!(pinned_endpoint(&link.join("worker.sock")).is_err());
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(pinned_endpoint(&socket).is_err());
        }
    }
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    match linux::run() {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::from(1),
        Err(_) => std::process::ExitCode::from(2),
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("cass-query requires Linux; ordinary cass search remains available.");
    std::process::ExitCode::from(2)
}
