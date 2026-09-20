//! Validate input when FAD emits no conversation and enrichment cannot run.
//!
//! The published legacy parser swallows read/JSON errors. The JSONL parser can
//! also drop an unfinished first record or invalid UTF-8 before yielding any
//! messages. A stable file is not proof of a successful read. This check only
//! runs on admitted zero-output sources; it does not normalize messages, infer
//! schema support, or fabricate completion events.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

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
        let mut content = String::new();
        let consumed = reader.read_to_string(&mut content)? as u64;
        serde_json::from_str::<Value>(content.trim_start_matches('\u{feff}'))
            .map_err(|error| invalid_json(&error))?;
        consumed
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
    // Do not embed a parser's unexpected token or any session text in errors.
    if error.is_eof() {
        io::Error::new(io::ErrorKind::UnexpectedEof, "unfinished Codex JSON source")
    } else {
        io::Error::new(io::ErrorKind::InvalidData, "invalid Codex JSON source")
    }
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
        if let Err(error) = serde_json::from_str::<Value>(text)
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
