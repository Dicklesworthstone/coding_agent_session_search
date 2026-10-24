//! Serialize monotone UTC buckets without retaining an index of their labels.
//!
//! Admission is an allocation-free pass over bucket numbers. Only four-digit,
//! unsigned years are admitted: their formatted keys have chronological lexical
//! order. The caller's indexed fallback handles signed/extended years, invalid
//! timestamps and interleaved groups without changing their historical order.

use chrono::{Datelike, TimeZone, Utc};
use serde::ser::{Error, SerializeMap};
use serde::{Serialize, Serializer};

use super::{Group, Session, granularity};
use crate::TimelineGrouping;

pub(super) struct OrderedGroups<'a> {
    pub(super) sessions: &'a [Session],
    pub(super) len: usize,
    width: i64,
    format: &'static str,
    descending: bool,
}

impl<'a> OrderedGroups<'a> {
    pub(super) fn new(sessions: &'a [Session], grouping: TimelineGrouping) -> Option<Self> {
        let (width, format) = granularity(grouping);
        let mut groups = Self {
            sessions,
            len: 0,
            width,
            format,
            descending: false,
        };
        let Some(first) = sessions.first() else {
            return Some(groups);
        };
        let last = sessions.last()?;
        for endpoint in [first, last] {
            let date = Utc.timestamp_millis_opt(endpoint.3).single()?;
            if !(0..=9999).contains(&date.year()) {
                return None;
            }
        }

        // Bucket monotonicity bounds every intermediate timestamp to valid
        // whole UTC days between the endpoints. It also rules out a repeated
        // noncontiguous group. Members inside one bucket need not be sorted:
        // they retain encounter order, including equal-start ties.
        let mut previous = first.3.div_euclid(width);
        groups.descending = previous > last.3.div_euclid(width);
        groups.len = 1;
        for session in &sessions[1..] {
            let bucket = session.3.div_euclid(width);
            if bucket == previous {
                continue;
            }
            if (bucket < previous) != groups.descending {
                return None;
            }
            groups.len += 1;
            previous = bucket;
        }
        Some(groups)
    }

    fn serialize_run<M: SerializeMap>(&self, map: &mut M, run: &[Session]) -> Result<(), M::Error> {
        let date = Utc
            .timestamp_millis_opt(run[0].3)
            .single()
            .ok_or_else(|| M::Error::custom("invalid timestamp in admitted timeline group"))?;
        let key = date.format(self.format).to_string();
        // Vec::new has no allocation. Reuse the exact existing member
        // serializer, including its provenance and duration-field policy.
        map.serialize_entry(
            &key,
            &Group {
                first: run,
                later: Vec::new(),
                len: run.len(),
            },
        )
    }
}

impl Serialize for OrderedGroups<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.len))?;
        let runs = self
            .sessions
            .chunk_by(|a, b| a.3.div_euclid(self.width) == b.3.div_euclid(self.width));
        if self.descending {
            // Reverse groups, never their members. This emits the same
            // ascending lexical map keys as BTreeMap while preserving the
            // database's descending session order inside each group.
            for run in runs.rev() {
                self.serialize_run(&mut map, run)?;
            }
        } else {
            for run in runs {
                self.serialize_run(&mut map, run)?;
            }
        }
        map.end()
    }
}
