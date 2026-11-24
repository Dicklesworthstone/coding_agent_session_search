//! Classify only the final complete physical line of a private parser view.
//!
//! The primary reader's pairing rule counts physical lines, including blanks
//! and malformed lines. Recovered records share one physical index, so the
//! last native-emitting record (not the last JSON value) determines whether an
//! event prompt can still pair with the next physical line.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};

use anyhow::Result;
use franken_agent_detection::connectors::jsonl::{
    MAX_JSONL_RECOVERY_LINE_BYTES, recover_jsonl_suffix,
};
use serde_json::Value;

use super::super::implementation::physical_line_ends_in_user_event;

pub(super) fn needs_next_record(file: &File, start: u64, end: u64) -> Result<bool> {
    let length = end.checked_sub(start).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "invalid Codex physical-line boundary")
    })?;
    let mut source = file;
    source.seek(SeekFrom::Start(start))?;
    // FAD accepts one or more BOMs at the beginning of every physical line.
    let mut parse_start = start;
    while end - parse_start >= 3 {
        let mut prefix = [0_u8; 3];
        source.read_exact(&mut prefix)?;
        if prefix == [0xef, 0xbb, 0xbf] {
            parse_start += 3;
        } else {
            source.seek(SeekFrom::Start(parse_start))?;
            break;
        }
    }
    let parsed: serde_json::Result<Value> =
        serde_json::from_reader(BufReader::new(source.take(end - parse_start)));
    match parsed {
        Ok(record) => Ok(physical_line_ends_in_user_event(std::slice::from_ref(&record))),
        Err(error) if error.is_io() => Err(error.into()),
        Err(_) => {
            // Recovery already has a byte budget. Do not allocate another
            // arbitrarily large line merely to classify its semantic tail.
            // A large unclassified last line is conservatively withheld until
            // another physical record or the existing inactivity policy.
            let recovery_limit = MAX_JSONL_RECOVERY_LINE_BYTES as u64;
            if length > recovery_limit.saturating_add(2) {
                return Ok(true);
            }
            source.seek(SeekFrom::Start(start))?;
            let mut line = String::new();
            source.take(length).read_to_string(&mut line)?;
            let physical = line.strip_suffix('\n').map_or(line.as_str(), |text| {
                text.strip_suffix('\r').unwrap_or(text)
            });
            // The main reader trims Unicode whitespace around intact JSON;
            // serde's streaming reader only recognizes JSON whitespace.
            if let Ok(record) =
                serde_json::from_str::<Value>(physical.trim_start_matches('\u{feff}').trim())
            {
                return Ok(physical_line_ends_in_user_event(std::slice::from_ref(&record)));
            }
            let recovered = recover_jsonl_suffix(physical);
            Ok(physical_line_ends_in_user_event(&recovered.records))
        }
    }
}
