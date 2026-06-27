use anyhow::{Context, Result, anyhow, bail};
use ftui::runtime::{AsciicastRecorder, AsciicastWriter};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, IsTerminal, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// Inline POSIX constants and FFI for fcntl / EIO — avoids a direct `libc` dependency.
#[cfg(unix)]
mod posix {
    use std::ffi::c_int;
    pub const EIO: c_int = 5;
    pub const F_GETFL: c_int = 3;
    pub const F_SETFL: c_int = 4;

    // O_NONBLOCK varies across platforms — must use the right constant.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    pub const O_NONBLOCK: c_int = 0x0004;
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    pub const O_NONBLOCK: c_int = 0o4000;

    unsafe extern "C" {
        pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    }
}

/// Run the current `cass tui` invocation inside a PTY and mirror output to an
/// asciicast v2 file.
///
/// This records terminal output only by default. Input bytes are intentionally
/// not captured to reduce accidental secret leakage (passwords/tokens typed in
/// the terminal are not serialized into the recording stream).
pub fn run_tui_with_asciicast(recording_path: &Path, interactive: bool) -> Result<()> {
    ensure_asciicast_output_available(recording_path)?;

    let (child_args, removed_flag) = strip_asciicast_args(std::env::args().skip(1));
    if !removed_flag {
        return Err(anyhow!(
            "internal error: --asciicast flag was not found in process arguments"
        ));
    }

    let exe_path = std::env::current_exe().context("resolve current executable path")?;
    let exe_str = exe_path
        .to_str()
        .ok_or_else(|| anyhow!("executable path is not valid UTF-8"))?;

    let (cols, rows) = detect_terminal_size();
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("open PTY for asciicast recording")?;

    let mut cmd = CommandBuilder::new(exe_str);
    for arg in child_args {
        cmd.arg(arg);
    }
    // Parent already handled update prompt check; avoid duplicate prompt in child.
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .context("spawn TUI child process for asciicast recording")?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("clone PTY reader for asciicast capture")?;

    let mut writer_keepalive = Some(
        pair.master
            .take_writer()
            .context("take PTY writer for input forwarding")?,
    );
    let mut stdin_forwarder: Option<std::thread::JoinHandle<()>> = None;
    let mut stdin_stop_requested: Option<Arc<AtomicBool>> = None;
    #[cfg(unix)]
    let mut _stdin_nonblocking_guard: Option<StdinNonBlockingGuard> = None;

    let allow_input = interactive
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && dotenvy::var("TUI_HEADLESS").is_err();

    let _raw_mode = RawModeGuard::new(allow_input)?;

    if allow_input && let Some(writer) = writer_keepalive.take() {
        let stop_requested = Arc::new(AtomicBool::new(false));
        #[cfg(unix)]
        {
            _stdin_nonblocking_guard = StdinNonBlockingGuard::new(io::stdin().as_raw_fd()).ok();
        }
        let stop_for_thread = Arc::clone(&stop_requested);
        stdin_forwarder = Some(std::thread::spawn(move || {
            forward_stdin(writer, stop_for_thread)
        }));
        stdin_stop_requested = Some(stop_requested);
    }

    let run_result: Result<_> = (|| {
        let recorder = open_asciicast_recorder_no_overwrite(recording_path, cols, rows)
            .with_context(|| format!("create asciicast file at {}", recording_path.display()))?;
        let mut mirror = AsciicastWriter::new(io::stdout(), recorder);

        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    mirror
                        .write_all(&buf[..n])
                        .context("write PTY output to terminal/asciicast mirror")?;
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if is_pty_eof_error(&err) => break,
                Err(err) => return Err(err).context("read PTY output"),
            }
        }

        let _ = mirror.finish().context("finalize asciicast recording")?;
        let _ = writer_keepalive.take();

        child
            .wait()
            .context("wait for TUI child process to exit after recording")
    })();

    if let Some(stop_requested) = stdin_stop_requested.take() {
        stop_requested.store(true, Ordering::Relaxed);
    }

    if let Some(handle) = stdin_forwarder.take() {
        #[cfg(unix)]
        {
            if _stdin_nonblocking_guard.is_some() || handle.is_finished() {
                let _ = handle.join();
            }
        }
        #[cfg(not(unix))]
        {
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
        // If stdin could not be switched to nonblocking and the reader is still
        // blocked, dropping the handle intentionally detaches.
    }

    let status = match run_result {
        Ok(status) => status,
        Err(err) => {
            let _ = writer_keepalive.take();
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };

    if !status.success() {
        bail!("TUI exited with non-zero status while recording: {status}");
    }
    Ok(())
}

fn open_asciicast_recorder_no_overwrite(
    recording_path: &Path,
    cols: u16,
    rows: u16,
) -> Result<AsciicastRecorder<BufWriter<File>>> {
    ensure_asciicast_output_available(recording_path)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(recording_path).with_context(|| {
        format!(
            "create asciicast output file without overwrite at {}",
            recording_path.display()
        )
    })?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| anyhow!("system clock is before Unix epoch: {err}"))?
        .as_secs()
        .try_into()
        .context("asciicast timestamp exceeds i64 range")?;
    AsciicastRecorder::with_writer(BufWriter::new(file), cols, rows, timestamp)
        .with_context(|| format!("write asciicast header to {}", recording_path.display()))
}

fn ensure_asciicast_output_available(path: &Path) -> Result<()> {
    if path.file_name().filter(|name| !name.is_empty()).is_none() {
        bail!(
            "asciicast output path must include a filename: {}",
            path.display()
        );
    }

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure_asciicast_parent(parent)?;

    match std::fs::symlink_metadata(path) {
        Ok(_) => bail!("asciicast output already exists: {}", path.display()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("inspect asciicast output {}", path.display()))
        }
    }
}

fn ensure_asciicast_parent(parent: &Path) -> Result<()> {
    ensure_parent_chain_has_no_symlinks(parent)?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create asciicast parent directory {}", parent.display()))?;
    ensure_parent_chain_has_no_symlinks(parent)
}

fn ensure_parent_chain_has_no_symlinks(path: &Path) -> Result<()> {
    let mut ancestors: Vec<PathBuf> = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect();
    ancestors.reverse();

    for ancestor in ancestors {
        match std::fs::symlink_metadata(&ancestor) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_symlink() {
                    if is_allowed_system_symlink_ancestor(&ancestor) {
                        continue;
                    }
                    bail!(
                        "asciicast output parent must not contain symlinks: {}",
                        ancestor.display()
                    );
                }
                if !file_type.is_dir() {
                    bail!(
                        "asciicast output parent is not a directory: {}",
                        ancestor.display()
                    );
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("inspect asciicast parent directory {}", ancestor.display())
                });
            }
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn is_allowed_system_symlink_ancestor(path: &Path) -> bool {
    path == Path::new("/var") || path == Path::new("/tmp")
}

#[cfg(not(target_os = "macos"))]
fn is_allowed_system_symlink_ancestor(_path: &Path) -> bool {
    false
}

fn detect_terminal_size() -> (u16, u16) {
    fn env_dim(key: &str) -> Option<u16> {
        dotenvy::var(key)
            .ok()
            .and_then(|raw| raw.trim().parse::<u16>().ok())
            .filter(|value| *value > 0)
    }

    let env_cols = env_dim("COLUMNS");
    let env_rows = env_dim("LINES");
    if let (Some(cols), Some(rows)) = (env_cols, env_rows) {
        return (cols, rows);
    }

    #[cfg(unix)]
    {
        if io::stdin().is_terminal() {
            let output = std::process::Command::new("stty").arg("size").output().ok();
            if let Some(output) = output
                && output.status.success()
                && let Ok(text) = String::from_utf8(output.stdout)
            {
                let mut parts = text.split_whitespace();
                if let (Some(rows), Some(cols)) = (parts.next(), parts.next())
                    && let (Ok(rows), Ok(cols)) = (rows.parse::<u16>(), cols.parse::<u16>())
                    && rows > 0
                    && cols > 0
                {
                    return (cols, rows);
                }
            }
        }
    }

    (120, 40)
}

fn forward_stdin(mut child_writer: Box<dyn Write + Send>, stop_requested: Arc<AtomicBool>) {
    let stdin = io::stdin();
    let mut stdin_lock = stdin.lock();
    let mut buf = [0_u8; 256];
    loop {
        if stop_requested.load(Ordering::Relaxed) {
            break;
        }
        match stdin_lock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if child_writer.write_all(&buf[..n]).is_err() {
                    break;
                }
                if child_writer.flush().is_err() {
                    break;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

fn strip_asciicast_args<I>(args: I) -> (Vec<String>, bool)
where
    I: IntoIterator<Item = String>,
{
    let mut out = Vec::new();
    let mut removed = false;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--asciicast" {
            removed = true;
            let _ = iter.next();
            continue;
        }
        if arg.starts_with("--asciicast=") {
            removed = true;
            continue;
        }
        out.push(arg);
    }
    (out, removed)
}

fn is_pty_eof_error(err: &io::Error) -> bool {
    if matches!(
        err.kind(),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        err.raw_os_error() == Some(posix::EIO)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

struct RawModeGuard {
    #[cfg(unix)]
    inner: Option<ftui_tty::RawModeGuard>,
}

impl RawModeGuard {
    fn new(enabled: bool) -> Result<Self> {
        #[cfg(unix)]
        {
            let inner = if enabled {
                Some(
                    ftui_tty::RawModeGuard::enter()
                        .context("enable raw mode for input passthrough")?,
                )
            } else {
                None
            };
            Ok(Self { inner })
        }
        #[cfg(not(unix))]
        {
            let _ = enabled;
            Ok(Self {})
        }
    }
}

#[cfg(unix)]
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = self.inner.take();
    }
}

#[cfg(unix)]
struct StdinNonBlockingGuard {
    fd: RawFd,
    old_flags: std::ffi::c_int,
}

#[cfg(unix)]
impl StdinNonBlockingGuard {
    fn new(fd: RawFd) -> io::Result<Self> {
        // SAFETY: fcntl does not outlive `fd` and is called with valid command
        // constants; errors are surfaced via last_os_error.
        let old_flags = unsafe { posix::fcntl(fd, posix::F_GETFL) };
        if old_flags < 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: same as above; we preserve and later restore original flags.
        let set_result = unsafe { posix::fcntl(fd, posix::F_SETFL, old_flags | posix::O_NONBLOCK) };
        if set_result < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self { fd, old_flags })
    }
}

#[cfg(unix)]
impl Drop for StdinNonBlockingGuard {
    fn drop(&mut self) {
        // SAFETY: best-effort restoration of original descriptor flags.
        unsafe {
            let _ = posix::fcntl(self.fd, posix::F_SETFL, self.old_flags);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_asciicast_output_available, is_pty_eof_error, open_asciicast_recorder_no_overwrite,
        strip_asciicast_args,
    };
    use std::io;
    use std::io::Write as _;

    #[test]
    fn strips_split_asciicast_flag_and_value() {
        let input = vec![
            "tui".to_string(),
            "--asciicast".to_string(),
            "demo.cast".to_string(),
            "--once".to_string(),
        ];
        let (args, removed) = strip_asciicast_args(input);
        assert!(removed);
        assert_eq!(args, vec!["tui", "--once"]);
    }

    #[test]
    fn strips_inline_asciicast_flag() {
        let input = vec![
            "tui".to_string(),
            "--asciicast=demo.cast".to_string(),
            "--data-dir".to_string(),
            "/tmp/cass".to_string(),
        ];
        let (args, removed) = strip_asciicast_args(input);
        assert!(removed);
        assert_eq!(args, vec!["tui", "--data-dir", "/tmp/cass"]);
    }

    #[test]
    fn leaves_unrelated_args_untouched() {
        let input = vec!["tui".to_string(), "--once".to_string()];
        let (args, removed) = strip_asciicast_args(input.clone());
        assert!(!removed);
        assert_eq!(args, input);
    }

    #[test]
    fn recognizes_common_pty_eof_errors() {
        let eof = io::Error::new(io::ErrorKind::UnexpectedEof, "eof");
        assert!(is_pty_eof_error(&eof));

        let pipe = io::Error::new(io::ErrorKind::BrokenPipe, "broken");
        assert!(is_pty_eof_error(&pipe));
    }

    #[test]
    fn creates_asciicast_parent_and_new_output() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let output_path = tmp.path().join("nested").join("demo.cast");

        let recorder =
            open_asciicast_recorder_no_overwrite(&output_path, 80, 24).expect("open recorder");
        let mut writer = recorder.finish().expect("finish recorder");
        writer.flush().expect("flush recorder");

        let contents = std::fs::read_to_string(&output_path).expect("read asciicast");
        assert!(
            contents.starts_with("{\"version\":2"),
            "unexpected asciicast header: {contents:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn creates_asciicast_output_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let output_path = tmp.path().join("demo.cast");

        let recorder =
            open_asciicast_recorder_no_overwrite(&output_path, 80, 24).expect("open recorder");
        let mut writer = recorder.finish().expect("finish recorder");
        writer.flush().expect("flush recorder");

        let mode = std::fs::metadata(&output_path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "asciicast recordings should not gain group/other permissions"
        );
    }

    #[test]
    fn rejects_existing_asciicast_output_without_clobbering() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let output_path = tmp.path().join("demo.cast");
        std::fs::write(&output_path, "existing cast").expect("seed existing output");

        let err = open_asciicast_recorder_no_overwrite(&output_path, 80, 24)
            .expect_err("existing output should be rejected");

        assert!(
            err.to_string().contains("already exists"),
            "unexpected error: {err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(&output_path).expect("read existing output"),
            "existing cast"
        );
    }

    #[test]
    #[cfg(unix)]
    fn rejects_existing_asciicast_output_symlink_without_following() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let protected_target = tmp.path().join("protected.cast");
        let output_path = tmp.path().join("demo.cast");
        std::fs::write(&protected_target, "protected").expect("seed protected target");
        symlink(&protected_target, &output_path).expect("create output symlink");

        let err = open_asciicast_recorder_no_overwrite(&output_path, 80, 24)
            .expect_err("symlink output should be rejected");

        assert!(
            err.to_string().contains("already exists"),
            "unexpected error: {err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(&protected_target).expect("read protected target"),
            "protected"
        );
        assert_eq!(
            std::fs::read_link(&output_path).expect("output path remains symlink"),
            protected_target
        );
    }

    #[test]
    #[cfg(unix)]
    fn rejects_symlinked_asciicast_parent_before_creating_output() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let outside_dir = tmp.path().join("outside");
        let linked_dir = tmp.path().join("linked");
        std::fs::create_dir_all(&outside_dir).expect("create outside dir");
        symlink(&outside_dir, &linked_dir).expect("create parent symlink");
        let output_path = linked_dir.join("demo.cast");

        let err = ensure_asciicast_output_available(&output_path)
            .expect_err("symlinked parent should be rejected");

        assert!(
            err.to_string().contains("must not contain symlinks"),
            "unexpected error: {err:#}"
        );
        assert!(
            !outside_dir.join("demo.cast").exists(),
            "preflight should not write through symlinked parent"
        );
    }
}
