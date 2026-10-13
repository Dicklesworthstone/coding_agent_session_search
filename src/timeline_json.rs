//! Borrowed JSON projection for timeline output (GH#521).
//!
//! The query still owns the selected session summaries. Do not clone their
//! strings into a second Value tree or buffer the complete encoded document.
//! Grouping borrows contiguous slices, retaining the original per-group order.

use std::io::{self, Write};

use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};

use crate::{
    TimelineGrouping, normalized_provenance_origin_host, normalized_provenance_origin_kind,
    normalized_provenance_source_id,
};

mod groups;

#[allow(clippy::type_complexity)]
pub(crate) type Session = (
    i64,
    String,
    Option<String>,
    i64,
    Option<i64>,
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
);

struct SessionView<'a> {
    session: &'a Session,
    include_duration: bool,
}

impl Serialize for SessionView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (id, agent, title, started, ended, path, count, source, host, kind) = self.session;
        let source = normalized_provenance_source_id(source, kind.as_deref(), host.as_deref());
        let kind = normalized_provenance_origin_kind(&source, kind.as_deref());
        let host = normalized_provenance_origin_host(host.as_deref());
        // Keep the lexicographic key order of the former serde_json::Value
        // projection, including the flat-only duration_seconds field.
        let mut map = serializer.serialize_map(Some(if self.include_duration { 11 } else { 10 }))?;
        map.serialize_entry("agent", agent)?;
        if self.include_duration {
            map.serialize_entry("duration_seconds", &ended.map(|end| (end - started) / 1000))?;
        }
        map.serialize_entry("ended_at", ended)?;
        map.serialize_entry("id", id)?;
        map.serialize_entry("message_count", count)?;
        map.serialize_entry("origin_host", &host)?;
        map.serialize_entry("origin_kind", &kind)?;
        map.serialize_entry("source_id", &source)?;
        map.serialize_entry("source_path", path)?;
        map.serialize_entry("started_at", started)?;
        map.serialize_entry("title", title)?;
        map.end()
    }
}

struct FlatSessions<'a>(&'a [Session]);

impl Serialize for FlatSessions<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for session in self.0 {
            sequence.serialize_element(&SessionView {
                session,
                include_duration: true,
            })?;
        }
        sequence.end()
    }
}

// Declaration order preserves the old Value object's sorted range keys.
#[derive(Serialize)]
struct Range {
    end: i64,
    start: i64,
}

struct Timeline<'a> {
    sessions: &'a [Session],
    range: Range,
    groups: Option<groups::Groups<'a>>,
}

impl Serialize for Timeline<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        if let Some(groups) = &self.groups {
            map.serialize_entry("groups", groups)?;
            map.serialize_entry("range", &self.range)?;
        } else {
            map.serialize_entry("range", &self.range)?;
            map.serialize_entry("sessions", &FlatSessions(self.sessions))?;
        }
        map.serialize_entry("total_sessions", &self.sessions.len())?;
        map.end()
    }
}

/// Write the existing JSON envelope, including its trailing newline.
///
/// Flat output needs no collection beyond the caller's summaries. Grouped
/// output indexes contiguous time buckets, not individual session references.
/// Other robot formats retain their existing projection/formatting path.
pub(crate) fn write<W: Write>(
    writer: &mut W,
    sessions: &[Session],
    start: i64,
    end: i64,
    grouping: TimelineGrouping,
    pretty: bool,
) -> io::Result<()> {
    let groups = match grouping {
        TimelineGrouping::None => None,
        TimelineGrouping::Hour | TimelineGrouping::Day => {
            Some(groups::Groups::new(sessions, grouping))
        }
    };
    let timeline = Timeline {
        sessions,
        range: Range { end, start },
        groups,
    };
    if pretty {
        serde_json::to_writer_pretty(&mut *writer, &timeline)?;
    } else {
        serde_json::to_writer(&mut *writer, &timeline)?;
    }
    writer.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    fn fixture() -> Vec<Session> {
        let start = 1_705_276_800_000_i64;
        vec![
            (9, "claude_code".into(), Some("quotes \" newline\n λ".into()), start + 86_400_000, None,
             "/new\\session.jsonl".into(), 0, "backup".into(), Some(" host ".into()), Some("local".into())),
            (4, "codex".into(), None, start + 3_600_000, Some(start + 3_620_999),
             "/remote.jsonl".into(), 900_001, "   ".into(), Some(" user@remote ".into()), None),
            (2, "unknown".into(), Some("older".into()), start, Some(start - 2_000),
             "/old.jsonl".into(), 3, "local".into(), None, None),
            (3, "codex".into(), Some("equal start".into()), start, Some(start + 20_000),
             "/tie.jsonl".into(), 2, "local".into(), Some("   ".into()), Some("local".into())),
        ]
    }

    // Independent reproduction of the pre-streaming Value projection: the
    // expected bytes are NOT derived by serializing our custom view types.
    fn legacy(sessions: &[Session], grouping: TimelineGrouping) -> Value {
        let item = |s: &Session| {
            let source = normalized_provenance_source_id(&s.7, s.9.as_deref(), s.8.as_deref());
            let kind = normalized_provenance_origin_kind(&source, s.9.as_deref());
            let mut value = json!({
                "id": s.0, "agent": s.1, "title": s.2,
                "started_at": s.3, "ended_at": s.4, "source_path": s.5,
                "message_count": s.6, "source_id": source, "origin_kind": kind,
                "origin_host": normalized_provenance_origin_host(s.8.as_deref()),
            });
            if grouping == TimelineGrouping::None {
                value["duration_seconds"] = json!(s.4.map(|end| (end - s.3) / 1000));
            }
            value
        };
        if grouping == TimelineGrouping::None {
            json!({"range": {"start": 10, "end": 20}, "total_sessions": sessions.len(),
                   "sessions": sessions.iter().map(item).collect::<Vec<_>>()})
        } else {
            let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
            for session in sessions {
                let date = Utc.timestamp_millis_opt(session.3).single().unwrap();
                let format = if grouping == TimelineGrouping::Hour { "%Y-%m-%d %H:00" } else { "%Y-%m-%d" };
                groups.entry(date.format(format).to_string()).or_default().push(item(session));
            }
            json!({"range": {"start": 10, "end": 20}, "total_sessions": sessions.len(), "groups": groups})
        }
    }

    #[test]
    fn output_is_byte_identical_to_the_previous_projection() {
        for sessions in [fixture(), Vec::new()] {
            for grouping in [TimelineGrouping::None, TimelineGrouping::Hour, TimelineGrouping::Day] {
                let expected = legacy(&sessions, grouping);
                for pretty in [false, true] {
                    let mut actual = Vec::new();
                    write(&mut actual, &sessions, 10, 20, grouping, pretty).unwrap();
                    let mut bytes = if pretty {
                        serde_json::to_vec_pretty(&expected).unwrap()
                    } else {
                        serde_json::to_vec(&expected).unwrap()
                    };
                    bytes.push(b'\n');
                    assert_eq!(actual, bytes, "grouping={grouping:?}, pretty={pretty}");
                }
            }
        }
    }

    #[test]
    fn grouping_preserves_ties_nulls_and_kind_first_provenance() {
        let sessions = fixture();
        let mut bytes = Vec::new();
        write(&mut bytes, &sessions, 10, 20, TimelineGrouping::Day, false).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        let new = &value["groups"]["2024-01-16"][0];
        assert_eq!(new["id"], 9);
        assert_eq!(new["source_id"], "backup");
        assert_eq!(new["origin_kind"], "local");
        assert!(new["ended_at"].is_null());
        assert!(new.get("duration_seconds").is_none());
        let old = value["groups"]["2024-01-15"].as_array().unwrap();
        assert_eq!(old.iter().map(|row| row["id"].as_i64().unwrap()).collect::<Vec<_>>(), vec![4, 2, 3]);
        assert_eq!(old[0]["source_id"], "user@remote");
        assert_eq!(old[0]["origin_kind"], "remote");
        assert!(old[0]["title"].is_null());
    }

    struct FailingWriter {
        remaining: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "test sink closed"));
            }
            let count = bytes.len().min(self.remaining);
            self.remaining -= count;
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    #[test]
    fn serialization_and_trailing_newline_failures_are_reported() {
        for grouping in [TimelineGrouping::None, TimelineGrouping::Hour, TimelineGrouping::Day] {
            let sessions = fixture();
            let mut bytes = Vec::new();
            write(&mut bytes, &sessions, 10, 20, grouping, false).unwrap();
            for remaining in [0, bytes.len() / 2, bytes.len() - 1] {
                let error = write(&mut FailingWriter { remaining }, &sessions, 10, 20, grouping, false).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
            }
        }
    }

    #[derive(Default)]
    struct CountingWriter {
        bytes: usize,
    }

    impl Write for CountingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    #[test]
    fn large_output_can_be_written_to_a_non_accumulating_sink() {
        let mut session = fixture().remove(0);
        session.2 = Some("x".repeat(4096));
        let sessions = vec![session; 8192];
        for grouping in [TimelineGrouping::None, TimelineGrouping::Hour] {
            let mut sink = CountingWriter::default();
            write(&mut sink, &sessions, 10, 20, grouping, false).unwrap();
            assert!(sink.bytes > 32 * 1024 * 1024);
        }
        // This is a large-output correctness test, NOT a process-RSS claim.
    }
}
