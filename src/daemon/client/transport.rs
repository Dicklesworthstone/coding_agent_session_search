//! Request-owned daemon transport. No helper thread outlives a timed-out call.
//!
//! The same monotonic budget covers lock admission, connect retries, framing,
//! writes, reads and decode admission. Filesystem/process syscalls and a running
//! serializer/deserializer cannot be forcibly preempted; checks reject their
//! late results. Socket operations themselves are nonblocking.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;

pub(super) use crate::daemon::protocol::MAX_FRAME_BYTES;
const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone, Copy)]
pub(super) struct Deadline {
    started: Instant,
    timeout: Duration,
}

impl Deadline {
    pub(super) fn new(timeout: Duration) -> Self {
        Self {
            started: Instant::now(),
            timeout,
        }
    }

    /// Restrict a phase without extending the enclosing request's deadline.
    pub(super) fn capped(&self, timeout: Duration) -> Self {
        Self {
            started: self.started,
            timeout: self
                .timeout
                .min(self.started.elapsed().saturating_add(timeout)),
        }
    }

    pub(super) fn remaining(&self) -> io::Result<Duration> {
        let remaining = self.timeout.saturating_sub(self.started.elapsed());
        if remaining.is_zero() {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon request deadline exceeded",
            ))
        } else {
            Ok(remaining)
        }
    }

    pub(super) fn check(&self) -> io::Result<()> {
        self.remaining().map(|_| ())
    }

    pub(super) fn pause(&self) -> io::Result<()> {
        std::thread::sleep(self.remaining()?.min(POLL_INTERVAL));
        self.check()
    }
}

/// Bound encoding before opening a connection or writing any request bytes.
/// Serializer::new preserves protocol::encode_message's compact tuple format.
pub(super) fn encode<T: Serialize>(message: &T, deadline: &Deadline) -> io::Result<Vec<u8>> {
    struct FrameWriter<'a> {
        bytes: Vec<u8>,
        deadline: &'a Deadline,
        failure: Option<io::ErrorKind>,
    }
    impl Write for FrameWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Err(error) = self.deadline.check() {
                self.failure = Some(error.kind());
                return Err(error);
            }
            let Some(total) = self.bytes.len().checked_add(bytes.len()) else {
                self.failure = Some(io::ErrorKind::InvalidInput);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "daemon request too large",
                ));
            };
            if total > MAX_FRAME_BYTES + 4 {
                self.failure = Some(io::ErrorKind::InvalidInput);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "daemon request too large",
                ));
            }
            if total > self.bytes.capacity() {
                let capacity = self
                    .bytes
                    .capacity()
                    .saturating_mul(2)
                    .max(total)
                    .min(MAX_FRAME_BYTES + 4);
                self.bytes
                    .try_reserve_exact(capacity - self.bytes.len())
                    .map_err(|_| {
                        self.failure = Some(io::ErrorKind::OutOfMemory);
                        io::Error::new(
                            io::ErrorKind::OutOfMemory,
                            "cannot allocate daemon request frame",
                        )
                    })?;
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.deadline.check()
        }
    }
    deadline.check()?;
    let mut writer = FrameWriter {
        bytes: vec![0; 4],
        deadline,
        failure: None,
    };
    if message
        .serialize(&mut rmp_serde::Serializer::new(&mut writer))
        .is_err()
    {
        return Err(io::Error::new(
            writer.failure.unwrap_or(io::ErrorKind::InvalidData),
            "daemon request encoding failed or exceeded its frame budget",
        ));
    }
    deadline.check()?;
    let len = u32::try_from(writer.bytes.len() - 4)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "daemon request too large"))?;
    writer.bytes[..4].copy_from_slice(&len.to_be_bytes());
    Ok(writer.bytes)
}

pub(super) fn read_exact(
    stream: &mut UnixStream,
    mut bytes: &mut [u8],
    deadline: &Deadline,
) -> io::Result<()> {
    // Also applies to socket-pair tests or a connection installed by a caller.
    stream.set_nonblocking(true)?;
    while !bytes.is_empty() {
        deadline.check()?;
        match stream.read(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "daemon closed a partial frame",
                ));
            }
            Ok(count) => bytes = &mut bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                deadline.pause()?
            }
            Err(error) => return Err(error),
        }
    }
    deadline.check()
}

pub(super) fn write_all(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    deadline: &Deadline,
) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    while !bytes.is_empty() {
        deadline.check()?;
        match stream.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "daemon accepted no request bytes",
                ));
            }
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                deadline.pause()?
            }
            Err(error) => return Err(error),
        }
    }
    deadline.check()
}

/// std has no nonblocking UnixStream constructor. This small FFI boundary owns
/// every descriptor immediately and passes only a checked pathname sockaddr.
/// All subsequent I/O and ownership use safe std APIs. No raw session bytes or
/// secret material reach this boundary.
#[allow(unsafe_code)]
pub(super) fn connect(path: &Path, deadline: &Deadline) -> io::Result<UnixStream> {
    deadline.check()?;
    // SAFETY: sockaddr_un is a C integer/byte-array structure; all-zero is valid
    // initialization before setting its family, length and pathname below.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid daemon socket pathname",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address.sun_path.iter_mut().zip(bytes) {
        *target = *source as libc::c_char;
    }
    let length = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        address.sun_len = u8::try_from(length).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "daemon socket pathname is too long",
            )
        })?;
    }
    let length = libc::socklen_t::try_from(length).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "daemon socket address is too long",
        )
    })?;

    loop {
        deadline.check()?;
        let stream = unconnected_stream()?;
        // SAFETY: the live sockaddr_un pointer is correctly aligned, initialized
        // and at least `length` bytes long. The descriptor is owned by `stream`.
        let result = unsafe {
            libc::connect(
                stream.as_raw_fd(),
                std::ptr::from_ref(&address).cast(),
                length,
            )
        };
        if result == 0 {
            deadline.check()?;
            return Ok(stream);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            // AF_UNIX EAGAIN can mean a full listen backlog, NOT a connecting
            // socket. Recreate before retrying; SO_ERROR=0 alone is not success.
            drop(stream);
            deadline.pause()?;
            continue;
        }
        if error.raw_os_error() != Some(libc::EINPROGRESS)
            && error.raw_os_error() != Some(libc::EALREADY)
            && error.kind() != io::ErrorKind::Interrupted
        {
            return Err(error);
        }
        loop {
            deadline.check()?;
            if let Some(error) = stream.take_error()? {
                return Err(error);
            }
            if stream.peer_addr().is_ok() {
                deadline.check()?;
                return Ok(stream);
            }
            deadline.pause()?;
        }
    }
}

#[allow(unsafe_code)]
fn unconnected_stream() -> io::Result<UnixStream> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = libc::SOCK_STREAM;
    // SAFETY: socket creates a fresh descriptor and retains no Rust pointers.
    let raw = unsafe { libc::socket(libc::AF_UNIX, flags, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socket returned a fresh, valid descriptor; ownership transfers
    // exactly once and all early errors subsequently drop/close it.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // SAFETY: fcntl operates on the live owned descriptor. Platforms without
        // atomic SOCK_CLOEXEC need this fallback (as with older std sockets).
        let current = unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_GETFD) };
        if current < 0 {
            return Err(io::Error::last_os_error());
        }
        let result =
            unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_SETFD, current | libc::FD_CLOEXEC) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let stream = UnixStream::from(owned);
    stream.set_nonblocking(true)?;
    // Rust's UnixStream writes suppress SIGPIPE on Linux. Apple also requires
    // SO_NOSIGPIPE when constructing the socket outside std::UnixStream::connect.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let enabled: libc::c_int = 1;
        // SAFETY: setsockopt copies a correctly sized live c_int; it does not
        // retain the pointer. The owned socket is valid throughout the call.
        let result = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                std::ptr::from_ref(&enabled).cast(),
                std::mem::size_of_val(&enabled) as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{FramedMessage, Request, encode_message};

    #[test]
    fn bounded_encoder_preserves_wire_bytes_and_rejects_oversize_before_io() {
        let deadline = Deadline::new(Duration::from_secs(10));
        for request in [
            Request::Health,
            Request::Embed {
                texts: vec!["résumé 日本語".into(), "ordered input".into()],
                model: "default".into(),
                dims: None,
            },
        ] {
            let framed = FramedMessage::new("wire-test", request);
            assert_eq!(
                encode(&framed, &deadline).unwrap(),
                encode_message(&framed).unwrap()
            );
        }
        let too_large = FramedMessage::new(
            "large",
            Request::EmbeddingJobStatus {
                db_path: "x".repeat(MAX_FRAME_BYTES),
            },
        );
        assert_eq!(
            encode(&too_large, &deadline).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            encode(&42, &Deadline::new(Duration::ZERO))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn trickled_bytes_do_not_renew_the_read_budget() -> io::Result<()> {
        let (mut client, mut server) = UnixStream::pair()?;
        let sender = std::thread::spawn(move || {
            for _ in 0..30 {
                if server.write_all(&[7]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let start = Instant::now();
        let error = read_exact(
            &mut client,
            &mut [0; 30],
            &Deadline::new(Duration::from_millis(100)),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_millis(500));
        drop(client);
        sender.join().unwrap();
        Ok(())
    }

    #[test]
    fn stalled_reader_cannot_extend_write_deadline() -> io::Result<()> {
        let (mut client, _server) = UnixStream::pair()?;
        let error = write_all(
            &mut client,
            &vec![0; MAX_FRAME_BYTES],
            &Deadline::new(Duration::from_millis(50)),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        Ok(())
    }

    #[test]
    fn fragmented_reads_preserve_every_byte_and_eof_is_not_a_timeout() -> io::Result<()> {
        let (mut client, mut server) = UnixStream::pair()?;
        let sender = std::thread::spawn(move || -> io::Result<()> {
            for bytes in [&b"ab"[..], &b"c"[..], &b"def"[..]] {
                server.write_all(bytes)?;
            }
            Ok(())
        });
        let deadline = Deadline::new(Duration::from_secs(2));
        let mut bytes = [0; 6];
        read_exact(&mut client, &mut bytes, &deadline)?;
        assert_eq!(&bytes, b"abcdef");
        assert_eq!(
            read_exact(&mut client, &mut [0], &deadline)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        sender.join().unwrap()?;
        Ok(())
    }

    #[test]
    fn connect_is_nonblocking_and_accepts_non_utf8_pathnames() -> io::Result<()> {
        use std::ffi::OsStr;
        let temp = tempfile::tempdir()?;
        let path = temp.path().join(OsStr::from_bytes(b"socket-\xff"));
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        let mut client = connect(&path, &Deadline::new(Duration::from_secs(2)))?;
        let (_server, _) = listener.accept()?;
        assert_eq!(
            client.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            connect(&path, &Deadline::new(Duration::ZERO))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        Ok(())
    }

    #[test]
    fn invalid_and_missing_socket_paths_fail_without_creating_files() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let deadline = Deadline::new(Duration::from_secs(1));
        assert_eq!(
            connect(Path::new(""), &deadline).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            connect(Path::new("a\0b"), &deadline).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            connect(&temp.path().join("missing"), &deadline)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(std::fs::read_dir(temp.path())?.count(), 0);
        Ok(())
    }
}
