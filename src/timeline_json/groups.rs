//! Index timeline groups by borrowed runs, not by individual sessions.
//!
//! The database orders summaries by descending start time, so each valid UTC
//! hour/day is one contiguous run. A large group therefore needs one slice,
//! not a growing Vec<SessionView>. Interleaved input remains supported without
//! sorting its members: later runs are appended in encounter order. Memory is
//! O(groups + runs), in addition to the caller-owned summaries and writer.

use std::collections::{BTreeMap, btree_map::Entry};

use chrono::{TimeZone, Utc};
use serde::ser::SerializeSeq;
use serde::{Serialize, Serializer};

use super::{Session, SessionView};
use crate::TimelineGrouping;

pub(super) struct Groups<'a>(BTreeMap<String, Group<'a>>);

struct Group<'a> {
    first: &'a [Session],
    // Empty (and unallocated) for the timestamp-ordered database result.
    later: Vec<&'a [Session]>,
    len: usize,
}

impl<'a> Groups<'a> {
    pub(super) fn new(sessions: &'a [Session], grouping: TimelineGrouping) -> Self {
        let (width, format) = match grouping {
            TimelineGrouping::Hour => (3_600_000, "%Y-%m-%d %H:00"),
            TimelineGrouping::Day => (86_400_000, "%Y-%m-%d"),
            TimelineGrouping::None => unreachable!("flat output does not build groups"),
        };
        let mut groups = Self(BTreeMap::new());
        // Euclidean division is essential before the Unix epoch: -1 ms
        // belongs to the preceding hour/day, not the bucket beginning at 0.
        for run in sessions.chunk_by(|a, b| a.3.div_euclid(width) == b.3.div_euclid(width)) {
            if let Some(date) = Utc.timestamp_millis_opt(run[0].3).single() {
                // A representable UTC date covers its entire day. Every
                // timestamp in this run is in the same UTC hour/day.
                groups.push(date.format(format).to_string(), run);
            } else {
                // Preserve the old per-session clock fallback for malformed
                // timestamps, including when processing crosses a boundary.
                for session in run {
                    groups.push(
                        Utc::now().format(format).to_string(),
                        std::slice::from_ref(session),
                    );
                }
            }
        }
        groups
    }

    fn push(&mut self, key: String, run: &'a [Session]) {
        match self.0.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(Group {
                    first: run,
                    later: Vec::new(),
                    len: run.len(),
                });
            }
            Entry::Occupied(mut entry) => {
                let group = entry.get_mut();
                group.len += run.len();
                group.later.push(run);
            }
        }
    }
}

impl Serialize for Groups<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Preserve lexical key order even for signed/extended calendar years,
        // where reversing chronological buckets is not equivalent.
        self.0.serialize(serializer)
    }
}

impl Serialize for Group<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.len))?;
        for run in std::iter::once(self.first).chain(self.later.iter().copied()) {
            for session in run {
                sequence.serialize_element(&SessionView {
                    session,
                    include_duration: false,
                })?;
            }
        }
        sequence.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    fn session(id: i64, started: i64) -> Session {
        (
            id,
            String::new(),
            None,
            started,
            None,
            String::new(),
            0,
            String::new(),
            None,
            None,
        )
    }

    // The previous grouping implementation, retained as a live control.
    // It deliberately does not call Groups::new or use its bucket arithmetic.
    fn reference(
        sessions: &[Session],
        grouping: TimelineGrouping,
    ) -> BTreeMap<String, Vec<SessionView<'_>>> {
        let format = match grouping {
            TimelineGrouping::Hour => "%Y-%m-%d %H:00",
            TimelineGrouping::Day => "%Y-%m-%d",
            TimelineGrouping::None => unreachable!(),
        };
        let mut groups: BTreeMap<String, Vec<SessionView<'_>>> = BTreeMap::new();
        for session in sessions {
            let date = Utc
                .timestamp_millis_opt(session.3)
                .single()
                .unwrap_or_else(Utc::now);
            groups
                .entry(date.format(format).to_string())
                .or_default()
                .push(SessionView {
                    session,
                    include_duration: false,
                });
        }
        groups
    }

    fn assert_equivalent(sessions: &[Session]) {
        for grouping in [TimelineGrouping::Hour, TimelineGrouping::Day] {
            let actual = Groups::new(sessions, grouping);
            let expected = reference(sessions, grouping);
            assert_eq!(
                serde_json::to_vec(&actual).unwrap(),
                serde_json::to_vec(&expected).unwrap()
            );
            assert_eq!(
                serde_json::to_vec_pretty(&actual).unwrap(),
                serde_json::to_vec_pretty(&expected).unwrap()
            );
        }
    }

    #[test]
    fn empty_input_has_no_groups() {
        let groups = Groups::new(&[], TimelineGrouping::Day);
        assert!(groups.0.is_empty());
        assert_eq!(serde_json::to_string(&groups).unwrap(), "{}");
    }

    #[test]
    fn utc_boundaries_and_negative_timestamps_match_the_previous_grouping() {
        let timestamps = [
            86_400_001,
            86_400_000,
            86_399_999,
            3_600_000,
            3_599_999,
            1,
            0,
            0,
            -1,
            -3_600_000,
            -3_600_001,
            -86_400_000,
            -86_400_001,
        ];
        let sessions: Vec<_> = timestamps
            .into_iter()
            .enumerate()
            .map(|(id, t)| session(i64::try_from(id).unwrap(), t))
            .collect();
        assert_equivalent(&sessions);
        let groups = Groups::new(&sessions, TimelineGrouping::Hour);
        let prior_hour = &groups.0["1969-12-31 23:00"];
        assert_eq!(
            prior_hour.first.iter().map(|s| s.3).collect::<Vec<_>>(),
            vec![-1, -3_600_000]
        );
    }

    #[test]
    fn interleaved_runs_and_timestamp_ties_keep_encounter_order() {
        let sessions = [
            session(9, 86_400_000),
            session(7, 0),
            session(4, 86_400_000),
            session(2, 0),
            session(1, 86_400_000),
        ];
        assert_equivalent(&sessions);
        let groups = Groups::new(&sessions, TimelineGrouping::Day);
        let later = &groups.0["1970-01-02"];
        assert_eq!(later.len, 3);
        assert_eq!(later.first[0].0, 9);
        assert_eq!(
            later.later.iter().map(|run| run[0].0).collect::<Vec<_>>(),
            vec![4, 1]
        );
    }

    #[test]
    fn signed_extended_years_and_chrono_limits_keep_lexical_key_order() {
        let mut sessions: Vec<_> = [10_000, 9_999, 1, 0, -1, -10_000]
            .into_iter()
            .map(|year| {
                let time = Utc
                    .with_ymd_and_hms(year, 1, 1, 0, 0, 0)
                    .single()
                    .unwrap()
                    .timestamp_millis();
                session(i64::from(year), time)
            })
            .collect();
        // Include both ends of the representable date domain and nearby
        // instants in the same day, testing the whole-day run assumption.
        let min = DateTime::<Utc>::MIN_UTC.timestamp_millis();
        let max = DateTime::<Utc>::MAX_UTC.timestamp_millis();
        sessions.extend([
            session(11, min),
            session(12, min + 1),
            session(13, max - 1),
            session(14, max),
        ]);
        assert_equivalent(&sessions);
        sessions.sort_by_key(|s| std::cmp::Reverse(s.3));
        assert_equivalent(&sessions);
    }

    #[test]
    fn malformed_timestamps_keep_each_session_in_the_clock_fallback() {
        let sessions = [
            session(3, i64::MAX),
            session(2, i64::MAX - 1),
            session(1, i64::MIN),
        ];
        for grouping in [TimelineGrouping::Hour, TimelineGrouping::Day] {
            let groups = Groups::new(&sessions, grouping);
            assert_eq!(
                groups.0.values().map(|g| g.len).sum::<usize>(),
                sessions.len()
            );
            for group in groups.0.values() {
                assert_eq!(group.first.len(), 1);
                assert!(group.later.iter().all(|run| run.len() == 1));
            }
            let value = serde_json::to_value(&groups).unwrap();
            let mut ids: Vec<_> = value
                .as_object()
                .unwrap()
                .values()
                .flat_map(|v| v.as_array().unwrap())
                .map(|v| v["id"].as_i64().unwrap())
                .collect();
            ids.sort_unstable();
            assert_eq!(ids, vec![1, 2, 3]);
        }
    }

    #[test]
    fn generated_ordered_and_interleaved_archives_match_the_live_control() {
        let mut seed = 0x0521_u64;
        let mut sessions: Vec<_> = (0..2048)
            .map(|id| {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let time = i64::try_from(seed % (120 * 86_400_000)).unwrap() - 60 * 86_400_000;
                session(id, time)
            })
            .collect();
        assert_equivalent(&sessions);
        sessions.sort_by_key(|s| std::cmp::Reverse(s.3));
        assert_equivalent(&sessions);
    }

    #[test]
    fn a_large_sorted_group_borrows_one_slice_without_a_per_session_index() {
        let sessions: Vec<_> = (0..65_536).map(|id| session(id, 0)).collect();
        for grouping in [TimelineGrouping::Hour, TimelineGrouping::Day] {
            let groups = Groups::new(&sessions, grouping);
            assert_eq!(groups.0.len(), 1);
            let group = groups.0.values().next().unwrap();
            assert!(std::ptr::eq(group.first, sessions.as_slice()));
            assert_eq!(group.len, sessions.len());
            assert_eq!(
                group.later.capacity(),
                0,
                "sorted input must not allocate a run vector"
            );
        }
    }

    #[cfg(target_os = "linux")]
    mod memory {
        use super::*;
        use std::io::{self, Write};

        #[derive(Default)]
        struct Sink(usize);

        impl Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0 += bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        fn peak_kib() -> u64 {
            let status = std::fs::read_to_string("/proc/self/status").unwrap();
            status
                .lines()
                .find_map(|line| {
                    line.strip_prefix("VmHWM:")?
                        .split_whitespace()
                        .next()?
                        .parse()
                        .ok()
                })
                .expect("Linux must expose a peak RSS sample")
        }

        fn run_probe(old: bool) {
            // Empty strings isolate the grouping index from message payloads.
            // The caller-owned summaries are fully constructed before baseline.
            let sessions: Vec<_> = (0..1_048_576).map(|id| session(id, 0)).collect();
            let baseline = peak_kib();
            let mut sink = Sink::default();
            if old {
                serde_json::to_writer(&mut sink, &reference(&sessions, TimelineGrouping::Day))
                    .unwrap();
            } else {
                serde_json::to_writer(&mut sink, &Groups::new(&sessions, TimelineGrouping::Day))
                    .unwrap();
            }
            let peak = peak_kib();
            println!("GH521_GROUP_RSS {} {}", peak.saturating_sub(baseline), sink.0);
        }

        #[test]
        #[ignore = "isolated child of grouped_json_memory_regression"]
        fn grouped_json_memory_child() {
            run_probe(false);
        }

        #[test]
        #[ignore = "isolated live control of grouped_json_memory_regression"]
        fn reference_grouping_memory_child() {
            run_probe(true);
        }

        fn measure(name: &str) -> (u64, usize) {
            let module = module_path!().split_once("::").unwrap().1;
            let output = assert_cmd::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!("{module}::{name}"),
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .timeout(std::time::Duration::from_secs(240))
                .assert()
                .success()
                .get_output()
                .stdout
                .clone();
            let text = String::from_utf8(output).unwrap();
            let mut fields = text
                .split("GH521_GROUP_RSS ")
                .nth(1)
                .expect("probe did not execute")
                .split_whitespace();
            (
                fields.next().unwrap().parse().unwrap(),
                fields.next().unwrap().parse().unwrap(),
            )
        }

        #[test]
        #[ignore = "million-session Linux RSS comparison; run explicitly with --ignored"]
        fn grouped_json_memory_regression() {
            // Fresh processes prevent allocator reuse/high-water history from
            // making one implementation look bounded merely by running second.
            let (new_kib, new_bytes) = measure("grouped_json_memory_child");
            let (old_kib, old_bytes) = measure("reference_grouping_memory_child");
            assert_eq!(new_bytes, old_bytes);
            assert!(
                new_bytes > 100 * 1024 * 1024,
                "probe must serialize substantial output"
            );
            assert!(
                old_kib >= 12 * 1024,
                "control did not expose its per-session index: {old_kib} KiB"
            );
            assert!(new_kib <= 8 * 1024, "grouped serializer grew by {new_kib} KiB");
            eprintln!("GH521 grouping: new={new_kib} KiB, live control={old_kib} KiB, bytes={new_bytes}");
        }
    }
}
