//! Validate input when FAD emits no conversation and enrichment cannot run.
//!
//! The published legacy parser swallows read/JSON errors. The JSONL parser can
//! also drop an unfinished first record or invalid UTF-8 before yielding any
//! messages. A stable file is not proof of a successful read. This check only
//! runs on admitted zero-output sources; it does not normalize messages, infer
//! schema support, or fabricate completion events.

mod syntax;

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result};

use super::{IncompleteScan, MAX_AUGMENT_ROLLOUT_BYTES, RejectedSource};

pub(super) fn validate(path: &Path, progress_tick: Option<&(dyn Fn() + Send + Sync)>) -> Result<()> {
    let file = File::open(path).context("open zero-output Codex source")?;
    let before = file.metadata().context("inspect zero-output Codex source")?;
    if !before.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zero-output Codex source is not a regular file",
        )
        .into());
    }
    if before.len() > MAX_AUGMENT_ROLLOUT_BYTES {
        return Err(IncompleteScan {
            rejected_source_count: 1,
            rejected_sources: vec![RejectedSource {
                source_path: path.to_string_lossy().into_owned(),
                observed_bytes: before.len(),
            }],
            ..IncompleteScan::default()
        }
        .into());
    }
    if let Some(tick) = progress_tick {
        tick();
    }
    // A finite opened prefix, not an unbounded read of a growing source.
    let mut reader = BufReader::new((&file).take(before.len()));
    let legacy = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
    let consumed = if legacy {
        validate_legacy(&mut reader)?;
        // from_reader checks trailing data and reaches EOF on success, so no
        // unread bytes remain in BufReader after a complete legacy document.
        before.len() - reader.get_ref().limit()
    } else {
        validate_jsonl(&mut reader, progress_tick)?
    };
    if consumed != before.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "zero-output Codex source was truncated while reading",
        )
        .into());
    }
    let opened = file.metadata().context("recheck zero-output Codex source")?;
    let named = fs::metadata(path).context("recheck zero-output Codex source path")?;
    if !super::super::same_rollout_snapshot(&before, &opened)?
        || !super::super::same_rollout_snapshot(&before, &named)?
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "zero-output Codex source changed while reading",
        )
        .into());
    }
    Ok(())
}

fn invalid_json(error: &serde_json::Error) -> io::Error {
    // Preserve actual reader error kinds, without embedding parser tokens or
    // session text in the source-failure diagnostics.
    if let Some(kind) = error.io_error_kind() {
        io::Error::new(kind, "Codex JSON source read failed")
    } else if error.is_eof() {
        io::Error::new(io::ErrorKind::UnexpectedEof, "unfinished Codex JSON source")
    } else {
        io::Error::new(io::ErrorKind::InvalidData, "invalid Codex JSON source")
    }
}

fn validate_legacy(reader: &mut impl BufRead) -> Result<()> {
    // Read the prefix through short reads too; fill_buf() is not guaranteed to
    // contain a complete BOM. Replay a non-BOM prefix without seeking the file.
    let mut prefix = Vec::with_capacity(3);
    (&mut *reader).take(3).read_to_end(&mut prefix)?;
    let prefix = prefix.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&prefix);
    let input = io::Cursor::new(prefix).chain(reader);
    serde_json::from_reader::<_, syntax::CheckedJson>(input)
        .map_err(|error| invalid_json(&error))?;
    Ok(())
}

fn validate_jsonl(
    reader: &mut impl BufRead,
    progress_tick: Option<&(dyn Fn() + Send + Sync)>,
) -> Result<u64> {
    let mut consumed = 0_u64;
    let mut line_no = 0_usize;
    let mut line = String::new();
    loop {
        line.clear();
        // read_line validates UTF-8; failures must not become successful EOF.
        let count = reader.read_line(&mut line)?;
        if count == 0 {
            return Ok(consumed);
        }
        consumed += count as u64;
        line_no += 1;
        if line_no.is_multiple_of(1024)
            && let Some(tick) = progress_tick
        {
            tick();
        }
        let terminated = line.ends_with('\n');
        let text = if line_no == 1 {
            line.trim_start_matches('\u{feff}').trim()
        } else {
            line.trim()
        };
        if text.is_empty() {
            continue;
        }
        if let Err(error) = serde_json::from_str::<syntax::CheckedJson>(text)
            && !terminated
            && error.is_eof()
        {
            return Err(invalid_json(&error).into());
        }
        // Match primary parsing/enrichment: malformed historical JSONL lines
        // are tolerated; a genuinely unfinished tail is retryable instead.
    }
}

#[cfg(test)]
mod tests;
