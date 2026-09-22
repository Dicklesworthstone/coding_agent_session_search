//! Bounded physical-line reads, including UTF-8 validation of discarded text.
//!
//! A skipped line is counted and validated without allocating its contents.
//! Checkpoints run between fixed-size chunks even when a line has no newline.
//! The caller supplies the existing request deadline, not a new per-line timer.

use std::io::{self, BufRead};

pub(super) const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_WINDOW_BYTES: usize = 32 * 1024 * 1024;
pub(super) const MAX_WINDOW_RECORDS: usize = 4096;
const CHUNK_BYTES: usize = 8192;

#[derive(Debug)]
pub(super) enum ReadError {
    Io(io::Error),
    RecordTooLarge,
}

impl From<io::Error> for ReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn invalid_utf8() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "source contains invalid UTF-8")
}

#[derive(Default)]
struct Utf8Validator {
    pending: [u8; 4],
    len: usize,
}

impl Utf8Validator {
    fn observe(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while self.len != 0 && !bytes.is_empty() {
            self.pending[self.len] = bytes[0];
            self.len += 1;
            bytes = &bytes[1..];
            match std::str::from_utf8(&self.pending[..self.len]) {
                Ok(_) => self.len = 0,
                Err(error) if error.error_len().is_none() => {}
                Err(_) => return Err(invalid_utf8()),
            }
        }
        if self.len == 0 {
            match std::str::from_utf8(bytes) {
                Ok(_) => {}
                Err(error) if error.error_len().is_none() => {
                    let tail = &bytes[error.valid_up_to()..];
                    self.pending[..tail.len()].copy_from_slice(tail);
                    self.len = tail.len();
                }
                Err(_) => return Err(invalid_utf8()),
            }
        }
        Ok(())
    }

    fn finish(&self) -> io::Result<()> {
        if self.len == 0 {
            Ok(())
        } else {
            Err(invalid_utf8())
        }
    }
}

/// `None` means EOF; `Some(None)` is a discarded but validated physical line.
/// Retained strings match `BufRead::lines`, including CRLF and final bare CR.
pub(super) fn read_line<R: BufRead>(
    reader: &mut R,
    retain: bool,
    max_bytes: usize,
    mut checkpoint: impl FnMut() -> io::Result<()>,
) -> Result<Option<Option<String>>, ReadError> {
    let mut text = Vec::new();
    let mut utf8 = Utf8Validator::default();
    let mut saw_bytes = false;
    loop {
        checkpoint()?;
        let available = match reader.fill_buf() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        checkpoint()?;
        if available.is_empty() {
            utf8.finish()?;
            if !saw_bytes {
                return Ok(None);
            }
            if retain && text.len() > max_bytes {
                return Err(ReadError::RecordTooLarge);
            }
            break;
        }
        // A custom BufRead may expose a huge slice. Do not let its buffer size
        // determine the amount of work between cancellation checkpoints.
        let chunk = &available[..available.len().min(CHUNK_BYTES)];
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let bytes = &chunk[..newline.unwrap_or(chunk.len())];
        utf8.observe(bytes)?;
        saw_bytes = true;
        if retain {
            let length = text
                .len()
                .checked_add(bytes.len())
                .ok_or(ReadError::RecordTooLarge)?;
            // Permit exactly one pending CR beyond the content cap so a CRLF
            // boundary does not reject an otherwise admissible line.
            let last = bytes.last().or_else(|| text.last());
            if length > max_bytes && !(length - max_bytes == 1 && last == Some(&b'\r')) {
                return Err(ReadError::RecordTooLarge);
            }
            text.extend_from_slice(bytes);
        }
        let consumed = bytes.len() + usize::from(newline.is_some());
        reader.consume(consumed);
        if newline.is_some() {
            utf8.finish()?;
            if retain && text.last() == Some(&b'\r') {
                text.pop();
            }
            break;
        }
    }
    checkpoint()?;
    if retain {
        let text = String::from_utf8(text).map_err(|_| invalid_utf8())?;
        Ok(Some(Some(text)))
    } else {
        Ok(Some(None))
    }
}

/// Bound actual retained data, not the untrusted requested context. A huge
/// context on a tiny transcript is still valid. Failed admission changes nothing.
#[derive(Default)]
pub(super) struct WindowSize {
    records: usize,
    bytes: usize,
}

impl WindowSize {
    pub fn admit(&mut self, bytes: usize) -> Result<(), &'static str> {
        if self.records >= MAX_WINDOW_RECORDS {
            return Err("follow-up window exceeds 4096 records");
        }
        let total = self
            .bytes
            .checked_add(bytes)
            .ok_or("window byte overflow")?;
        if total > MAX_WINDOW_BYTES {
            return Err("follow-up window exceeds 32 MiB of text");
        }
        self.records += 1;
        self.bytes = total;
        Ok(())
    }

    pub fn release(&mut self, bytes: usize) {
        self.records -= 1;
        self.bytes -= bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor, Read};

    #[test]
    fn chunk_boundaries_match_standard_lines_and_validate_discarded_utf8() {
        for input in ["", "\n", "\r", "a\r\n\nβ😀\r\ntail\r", "漢字\nlast"] {
            let expected: Vec<_> = Cursor::new(input)
                .lines()
                .collect::<io::Result<_>>()
                .unwrap();
            for capacity in 1..=9 {
                for retain in [true, false] {
                    let mut reader = BufReader::with_capacity(capacity, input.as_bytes());
                    let mut actual = Vec::new();
                    while let Some(line) = read_line(&mut reader, retain, 1024, || Ok(())).unwrap()
                    {
                        actual.push(line);
                    }
                    let expected: Vec<_> = expected
                        .iter()
                        .map(|line| retain.then(|| line.clone()))
                        .collect();
                    assert_eq!(actual, expected, "capacity={capacity}, input={input:?}");
                }
            }
        }
    }

    #[test]
    fn invalid_utf8_is_not_hidden_in_skipped_or_unterminated_lines() {
        for input in [
            &b"\xff\n"[..],
            &b"\xe2\x82"[..],
            &b"\xe2\x82\n"[..],
            &b"\xc0\x80\n"[..],
            &b"\xed\xa0\x80\n"[..],
            &b"\xf4\x90\x80\x80\n"[..],
        ] {
            for capacity in 1..=5 {
                for retain in [true, false] {
                    let mut reader = BufReader::with_capacity(capacity, input);
                    assert!(matches!(read_line(&mut reader, retain, 1024, || Ok(())),
                        Err(ReadError::Io(error)) if error.kind() == io::ErrorKind::InvalidData));
                }
            }
        }
    }

    #[test]
    fn retained_line_limit_accounts_for_crlf_but_not_bare_cr() {
        for capacity in 1..=8 {
            for (input, accepted) in [
                ("1234", true),
                ("1234\n", true),
                ("1234\r\n", true),
                ("1234\r", false),
                ("12345", false),
                ("12345\n", false),
                ("1234\rX\n", false),
            ] {
                let mut reader = BufReader::with_capacity(capacity, input.as_bytes());
                let result = read_line(&mut reader, true, 4, || Ok(()));
                if accepted {
                    assert_eq!(result.unwrap(), Some(Some("1234".into())));
                } else {
                    assert!(matches!(result, Err(ReadError::RecordTooLarge)));
                }
            }
        }
    }

    #[test]
    fn discarded_large_line_is_not_materialized_and_does_not_hide_next_line() {
        let source = io::repeat(b'x')
            .take((MAX_RECORD_BYTES * 3) as u64)
            .chain(Cursor::new(b"\ntarget\n"));
        let mut reader = BufReader::new(source);
        assert!(matches!(
            read_line(&mut reader, false, 4, || Ok(())),
            Ok(Some(None))
        ));
        assert_eq!(
            read_line(&mut reader, true, 6, || Ok(())).unwrap(),
            Some(Some("target".into()))
        );
        assert!(
            read_line(&mut reader, false, 4, || Ok(()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cancellation_stops_consumption_even_inside_a_line_without_newlines() {
        for retain in [false, true] {
            let mut reader = BufReader::new(io::repeat(b'x'));
            let mut checks = 0;
            let result = read_line(&mut reader, retain, MAX_RECORD_BYTES, || {
                checks += 1;
                if checks == 5 {
                    Err(io::Error::new(io::ErrorKind::TimedOut, "request deadline"))
                } else {
                    Ok(())
                }
            });
            assert!(
                matches!(result, Err(ReadError::Io(error)) if error.kind() == io::ErrorKind::TimedOut)
            );
            assert_eq!(
                checks, 5,
                "cancellation must terminate the reader, not detach it"
            );
        }
    }

    #[test]
    fn expired_request_does_not_start_io() {
        struct NoRead;
        impl Read for NoRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("expired request must not start a read");
            }
        }
        let mut reader = BufReader::new(NoRead);
        assert!(matches!(read_line(&mut reader, true, 4, || {
            Err(io::Error::new(io::ErrorKind::TimedOut, "expired"))
        }), Err(ReadError::Io(error)) if error.kind() == io::ErrorKind::TimedOut));
    }

    #[test]
    fn window_admission_is_bounded_and_rolling_release_is_exact() {
        let mut size = WindowSize::default();
        size.admit(MAX_WINDOW_BYTES).unwrap();
        assert!(size.admit(1).is_err());
        size.release(MAX_WINDOW_BYTES);
        for _ in 0..MAX_WINDOW_RECORDS {
            size.admit(0).unwrap();
        }
        assert!(size.admit(0).is_err());
        size.release(0);
        size.admit(1).unwrap();
        assert_eq!(size.records, MAX_WINDOW_RECORDS);
        assert_eq!(size.bytes, 1);
    }
}
