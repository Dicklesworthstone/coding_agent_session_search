//! Single-session streaming-OOM quarantine state (#243).
//!
//! When `cass index` (non-watch) encounters an irreducible streaming
//! OOM on a single conversation after deferred-lexical retry, the
//! policy is **quarantine-and-continue**: record the poison session,
//! advance the refresh for the rest of the corpus, and surface
//! `quarantined_conversations=N` so operators see it.
//!
//! The critical correctness property is **deduplication by conversation
//! identity**, not by occurrence: every refresh tick that hits the same
//! poison session must update the existing record's `last_attempt_at`
//! and `attempt_count`, not append a fresh entry. Without that, repeated
//! refreshes on a hot poison session would unbounded-grow the
//! quarantine state file and obscure which sessions are genuinely new
//! failures.
//!
//! Storage format: `<data_dir>/quarantine_state.json`, an object keyed
//! by `(conversation_id, schema_version)` so a schema bump that might
//! make a previously-poison session indexable again produces a fresh
//! quarantine record rather than coalescing with the stale one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Identity of a quarantined conversation, used as the dedup key.
///
/// `schema_version` is folded in so a future schema bump that changes
/// streaming-consumer memory pressure (e.g. a new message-format
/// encoding that no longer OOMs on the same conversation) produces a
/// fresh attempt rather than perpetually inheriting the prior verdict.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct QuarantineKey {
    pub conversation_id: String,
    pub schema_version: u32,
}

impl QuarantineKey {
    #[must_use]
    pub fn new(conversation_id: impl Into<String>, schema_version: u32) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            schema_version,
        }
    }

    fn storage_key(&self) -> String {
        format!("{}::v{}", self.conversation_id, self.schema_version)
    }

    fn parse_storage_key(key: &str) -> Option<Self> {
        let (conversation_id, version_part) = key.rsplit_once("::v")?;
        let schema_version: u32 = version_part.parse().ok()?;
        Some(Self {
            conversation_id: conversation_id.to_string(),
            schema_version,
        })
    }
}

/// One quarantine record, identified by [`QuarantineKey`].
///
/// `first_attempt_at` is preserved across repeated refreshes; only
/// `last_attempt_at`, `attempt_count`, and `last_reason` advance on
/// each occurrence. This is the contract that prevents the
/// "appending duplicate quarantine records" anti-pattern flagged on
/// #243.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineRecord {
    pub first_attempt_at: DateTime<Utc>,
    pub last_attempt_at: DateTime<Utc>,
    pub attempt_count: u64,
    pub last_reason: String,
    /// Cass version that produced the quarantine entry.
    ///
    /// `None` means the record pre-dates v0.6.x (written by v0.5.x or
    /// earlier), which lacked this field entirely.  The field is
    /// `#[serde(default)]` so missing-on-disk JSON is silently treated as
    /// `None` rather than a deserialisation error — critical for the legacy
    /// carry-over path described in cass#258.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cass_version_at_quarantine: Option<String>,
}

impl QuarantineRecord {
    /// Returns `true` when this record should be re-attempted on the next
    /// broad scan under `current_version`.
    ///
    /// The rule is:
    /// - `None`  → **legacy entry** (pre-v0.6.x, no version recorded) — always
    ///   retry-eligible, because the bug that originally caused the OOM may
    ///   already be fixed in the current binary.  Treating `None` as "same
    ///   version" would silently orphan every v0.5.x quarantine entry forever,
    ///   which is the cass#258 carry-over bug this method closes.
    /// - `Some(v)` where `v == current_version` → already retried under this
    ///   binary; **not** eligible (avoid infinite retry storms).
    /// - `Some(v)` where `v != current_version` → **stale entry**; a version
    ///   bump may have fixed the underlying bug, so retry once.
    #[must_use]
    pub fn is_version_stale_for_retry(&self, current_version: &str) -> bool {
        !matches!(&self.cass_version_at_quarantine, Some(v) if v == current_version)
    }
}

/// In-memory view of the quarantine state file. Use [`QuarantineState::load`]
/// to read, [`QuarantineState::record_attempt`] / [`QuarantineState::clear`]
/// to mutate, and [`QuarantineState::save`] to atomically persist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineState {
    /// Storage version of the file format itself (not the schema_version
    /// inside the keys). Bumped when the on-disk shape changes.
    #[serde(default = "default_storage_version")]
    pub storage_version: u32,
    /// Keyed by `QuarantineKey::storage_key()` for stable JSON ordering.
    pub entries: BTreeMap<String, QuarantineRecord>,
}

fn default_storage_version() -> u32 {
    1
}

impl Default for QuarantineState {
    fn default() -> Self {
        Self {
            storage_version: default_storage_version(),
            entries: BTreeMap::new(),
        }
    }
}

impl QuarantineState {
    /// Filename used inside `data_dir`. Stable on disk so external
    /// tooling can locate the state.
    pub const FILENAME: &'static str = "quarantine_state.json";

    #[must_use]
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(Self::FILENAME)
    }

    /// Load the quarantine state from `<data_dir>/quarantine_state.json`.
    /// Returns an empty state when the file is missing or malformed —
    /// quarantine is best-effort metadata and an unreadable state file
    /// must not block indexing.
    #[must_use]
    pub fn load(data_dir: &Path) -> Self {
        let path = Self::path(data_dir);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self {
                storage_version: 1,
                entries: BTreeMap::new(),
            };
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(state) => state,
            Err(_) => Self {
                storage_version: 1,
                entries: BTreeMap::new(),
            },
        }
    }

    /// Atomically write the quarantine state to disk via temp file + rename,
    /// so partial writes can never produce a corrupt quarantine_state.json.
    pub fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let final_path = Self::path(data_dir);
        let tmp_path = data_dir.join(format!("{}.tmp", Self::FILENAME));
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp_path, json)?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    /// Record an attempt that failed irreducibly on `key`. If the key
    /// already exists, the existing record is **updated in place**
    /// (`last_attempt_at`, `attempt_count`, `last_reason`) rather than
    /// appended — this is the dedup contract from #243.
    pub fn record_attempt(
        &mut self,
        key: &QuarantineKey,
        reason: impl Into<String>,
        now: DateTime<Utc>,
    ) {
        let reason = reason.into();
        let storage_key = key.storage_key();
        if let Some(record) = self.entries.get_mut(&storage_key) {
            record.last_attempt_at = now;
            record.attempt_count = record.attempt_count.saturating_add(1);
            record.last_reason = reason;
            record.cass_version_at_quarantine = Some(current_cass_version().to_string());
        } else {
            self.entries.insert(
                storage_key,
                QuarantineRecord {
                    first_attempt_at: now,
                    last_attempt_at: now,
                    attempt_count: 1,
                    last_reason: reason,
                    cass_version_at_quarantine: Some(current_cass_version().to_string()),
                },
            );
        }
    }

    /// Remove a quarantine entry. Called by `cass quarantine clear`
    /// after the operator has confirmed the underlying issue is
    /// resolved (e.g. a memory bump on the streaming consumer, a
    /// schema fix, or accepting the loss).
    pub fn clear(&mut self, key: &QuarantineKey) -> bool {
        self.entries.remove(&key.storage_key()).is_some()
    }

    /// Number of currently-quarantined `(conversation_id, schema_version)`
    /// keys. This is what `cass health` and the indexer JSON envelope
    /// surface as `quarantined_conversations`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate over `(key, record)` pairs in deterministic order.
    pub fn iter(&self) -> impl Iterator<Item = (QuarantineKey, &QuarantineRecord)> + '_ {
        self.entries.iter().filter_map(|(storage_key, record)| {
            QuarantineKey::parse_storage_key(storage_key).map(|k| (k, record))
        })
    }
}

fn current_cass_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use tempfile::tempdir;

    type TestResult = Result<(), Box<dyn Error>>;

    fn test_error(message: impl Into<String>) -> Box<dyn Error> {
        std::io::Error::other(message.into()).into()
    }

    fn ensure(condition: bool, message: impl Into<String>) -> TestResult {
        if condition {
            Ok(())
        } else {
            Err(test_error(message))
        }
    }

    fn ts(seconds: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(seconds, 0).expect("valid timestamp")
    }

    #[test]
    fn record_attempt_dedups_by_conversation_and_schema_version() {
        let mut state = QuarantineState::default();
        assert_eq!(state.storage_version, 1);
        let key = QuarantineKey::new("conv-a", 3);
        state.record_attempt(&key, "streaming-oom: 4.2 GB", ts(1_700_000_000));
        state.record_attempt(&key, "streaming-oom: 4.3 GB", ts(1_700_001_000));
        state.record_attempt(&key, "streaming-oom: 4.1 GB", ts(1_700_002_000));

        assert_eq!(state.len(), 1, "same key must dedup, not append");
        let record = state
            .entries
            .get(&key.storage_key())
            .expect("entry present");
        assert_eq!(
            record.first_attempt_at,
            ts(1_700_000_000),
            "first attempt preserved"
        );
        assert_eq!(
            record.last_attempt_at,
            ts(1_700_002_000),
            "last attempt advances"
        );
        assert_eq!(record.attempt_count, 3);
        assert_eq!(record.last_reason, "streaming-oom: 4.1 GB");
    }

    #[test]
    fn record_attempt_treats_different_schema_versions_as_distinct_keys() {
        let mut state = QuarantineState::default();
        let v3 = QuarantineKey::new("conv-a", 3);
        let v4 = QuarantineKey::new("conv-a", 4);
        state.record_attempt(&v3, "oom v3", ts(1));
        state.record_attempt(&v4, "oom v4", ts(2));
        assert_eq!(state.len(), 2, "schema bump must produce a fresh entry");
    }

    #[test]
    fn save_and_load_roundtrips_quarantine_state() {
        let dir = tempdir().unwrap();
        let mut state = QuarantineState::default();
        state.record_attempt(&QuarantineKey::new("c1", 1), "r1", ts(100));
        state.record_attempt(&QuarantineKey::new("c2", 1), "r2", ts(200));
        state.save(dir.path()).expect("save");

        let loaded = QuarantineState::load(dir.path());
        assert_eq!(loaded.len(), 2);
        let r1 = loaded
            .entries
            .get(&QuarantineKey::new("c1", 1).storage_key())
            .unwrap();
        assert_eq!(r1.last_reason, "r1");
    }

    #[test]
    fn load_returns_empty_for_missing_or_malformed_file() {
        let dir = tempdir().unwrap();
        let loaded = QuarantineState::load(dir.path());
        assert!(loaded.is_empty());

        std::fs::write(dir.path().join(QuarantineState::FILENAME), "not json")
            .expect("write malformed");
        let loaded = QuarantineState::load(dir.path());
        assert!(loaded.is_empty(), "malformed file must not block indexing");
    }

    #[test]
    fn clear_removes_entry() {
        let mut state = QuarantineState::default();
        let key = QuarantineKey::new("c", 1);
        state.record_attempt(&key, "r", ts(1));
        assert!(state.clear(&key));
        assert!(state.is_empty());
        assert!(!state.clear(&key), "clearing absent key returns false");
    }

    #[test]
    fn save_uses_atomic_rename_via_tmp_file() {
        let dir = tempdir().unwrap();
        let mut state = QuarantineState::default();
        state.record_attempt(&QuarantineKey::new("c", 1), "r", ts(1));
        state.save(dir.path()).expect("save");

        // The tmp file must not be left behind after a successful save.
        let tmp_path = dir
            .path()
            .join(format!("{}.tmp", QuarantineState::FILENAME));
        assert!(
            !tmp_path.exists(),
            "tmp file must be renamed away on success"
        );
        assert!(QuarantineState::path(dir.path()).exists());
    }

    #[test]
    fn iter_yields_keys_in_deterministic_order() {
        let mut state = QuarantineState::default();
        state.record_attempt(&QuarantineKey::new("c2", 1), "r2", ts(2));
        state.record_attempt(&QuarantineKey::new("c1", 1), "r1", ts(1));
        let ids: Vec<String> = state.iter().map(|(k, _)| k.conversation_id).collect();
        // BTreeMap-backed: ordering is by storage_key, which sorts c1 before c2.
        assert_eq!(ids, vec!["c1".to_string(), "c2".to_string()]);
    }

    /// Regression for cass#258 carry-over: v0.5.x entries written without
    /// `cass_version_at_quarantine` must deserialise cleanly (field becomes
    /// `None` via `#[serde(default)]`) and be considered retry-eligible so
    /// they are not silently orphaned forever.
    ///
    /// The fixture JSON deliberately omits the field, simulating a real
    /// `quarantine_state.json` produced by cass ≤ 0.5.1.
    #[test]
    fn legacy_entry_missing_cass_version_deserialises_and_is_retry_eligible() -> TestResult {
        let dir = tempdir()?;

        // Write a minimal quarantine_state.json that looks like v0.5.x output:
        // the `cass_version_at_quarantine` key is entirely absent.
        let legacy_json = serde_json::json!({
            "storage_version": 1,
            "entries": {
                "conv-legacy::v1": {
                    "first_attempt_at": "2025-11-01T00:00:00Z",
                    "last_attempt_at": "2025-11-01T00:00:00Z",
                    "attempt_count": 1,
                    "last_reason": "index-ingest-out-of-memory: out of memory"
                    // cass_version_at_quarantine intentionally absent
                }
            }
        });
        std::fs::write(
            dir.path().join(QuarantineState::FILENAME),
            serde_json::to_string_pretty(&legacy_json)?,
        )?;

        let state = QuarantineState::load(dir.path());
        ensure(
            state.len() == 1,
            format!(
                "legacy entry must load without error; loaded {} entries",
                state.len()
            ),
        )?;

        let record = state
            .entries
            .values()
            .next()
            .ok_or_else(|| test_error("entry present after loading legacy fixture"))?;

        ensure(
            record.cass_version_at_quarantine.is_none(),
            "missing field must deserialise as None, not cause an error",
        )?;

        // The critical correctness property: a legacy entry (None version) is
        // ALWAYS retry-eligible regardless of what the current binary version is.
        ensure(
            record.is_version_stale_for_retry("0.6.6"),
            "legacy entry with cass_version_at_quarantine=None must be retry-eligible \
             (cass#258 carry-over: v0.5.x entries were silently orphaned)",
        )?;
        ensure(
            record.is_version_stale_for_retry("0.5.1"),
            "legacy entry must be retry-eligible even when version string matches a v0.5.x tag",
        )?;
        ensure(
            record.is_version_stale_for_retry("99.0.0"),
            "legacy entry must be retry-eligible for any future version string",
        )?;
        Ok(())
    }

    #[test]
    fn versioned_entry_retry_eligibility_gates_correctly() -> TestResult {
        let current = current_cass_version();
        let mut state = QuarantineState::default();
        state.record_attempt(
            &QuarantineKey::new("conv-v", 1),
            "index-ingest-out-of-memory",
            ts(1),
        );
        // record_attempt always stamps cass_version_at_quarantine with the
        // current binary version; simulate "same version" by leaving as-is.
        let record = state
            .entries
            .values()
            .next()
            .ok_or_else(|| test_error("same-version quarantine record exists"))?;
        // A record stamped with the current version must NOT trigger a retry
        // (already tried under this binary).
        ensure(
            !record.is_version_stale_for_retry(current),
            "record stamped with current version must not be retry-eligible",
        )?;

        // Simulate a record written by an older version.
        let mut state2 = QuarantineState::default();
        state2.record_attempt(
            &QuarantineKey::new("conv-old", 1),
            "index-ingest-out-of-memory",
            ts(2),
        );
        state2
            .entries
            .values_mut()
            .next()
            .ok_or_else(|| test_error("old-version quarantine record exists"))?
            .cass_version_at_quarantine = Some("0.5.1".to_string());
        let old_record = state2
            .entries
            .values()
            .next()
            .ok_or_else(|| test_error("old-version quarantine record still exists"))?;
        ensure(
            old_record.is_version_stale_for_retry(current),
            "record stamped with older version must be retry-eligible after a version bump",
        )?;
        Ok(())
    }
}

/// Frozen compatibility/migration fixtures for the legacy and current
/// `quarantine_state.json` formats (bead
/// cass-fleet-resilience-20260608-uojcg.3.4).
///
/// These checked-in golden JSON files pin the on-disk shapes so future
/// changes cannot silently recreate cass#258 (legacy entries orphaned)
/// or the #243 duplicate-append anti-pattern. Each fixture is small and
/// redacted (synthetic `conv-*` ids, no real paths or user data) and is
/// loaded through the real [`QuarantineState::load`] path so the tests
/// exercise the production deserialiser, not a hand-rolled parser.
#[cfg(test)]
mod compat_fixtures {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::{TempDir, tempdir};

    const V05X_MISSING_VERSION: &str =
        include_str!("fixtures/quarantine/v05x_missing_version.json");
    const V06X_SAME_VERSION: &str = include_str!("fixtures/quarantine/v06x_same_version.json");
    const OLD_VERSION: &str = include_str!("fixtures/quarantine/old_version.json");
    const MALFORMED: &str = include_str!("fixtures/quarantine/malformed.json");
    const DUPLICATE_COLLAPSED: &str = include_str!("fixtures/quarantine/duplicate_collapsed.json");
    const SCHEMA_VERSION_BUMP: &str = include_str!("fixtures/quarantine/schema_version_bump.json");
    const GROUPING_MATRIX: &str = include_str!("fixtures/quarantine/grouping_matrix.json");

    /// All well-formed fixtures (everything except the deliberately
    /// malformed one), used by the roundtrip / determinism tests.
    const WELL_FORMED: &[(&str, &str)] = &[
        ("v05x_missing_version", V05X_MISSING_VERSION),
        ("v06x_same_version", V06X_SAME_VERSION),
        ("old_version", OLD_VERSION),
        ("duplicate_collapsed", DUPLICATE_COLLAPSED),
        ("schema_version_bump", SCHEMA_VERSION_BUMP),
        ("grouping_matrix", GROUPING_MATRIX),
    ];

    /// Write a fixture's JSON into a fresh data dir and load it through the
    /// production [`QuarantineState::load`] path. The `TempDir` is returned
    /// so callers that re-save keep the directory alive.
    fn load_fixture(json: &str) -> (TempDir, QuarantineState) {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join(QuarantineState::FILENAME), json).expect("write fixture");
        let state = QuarantineState::load(dir.path());
        (dir, state)
    }

    #[test]
    fn legacy_v05x_fixture_loads_and_is_retry_eligible() {
        let (_dir, state) = load_fixture(V05X_MISSING_VERSION);
        assert_eq!(state.len(), 1);
        let (key, record) = state.iter().next().expect("one legacy entry");
        assert_eq!(key.conversation_id, "conv-legacy");
        assert_eq!(key.schema_version, 1);
        assert!(
            record.cass_version_at_quarantine.is_none(),
            "missing field must deserialise as None (cass#258 carry-over)"
        );
        // Legacy entries are retry-eligible for ANY current version.
        for v in ["0.5.1", "0.6.13", "99.0.0"] {
            assert!(
                record.is_version_stale_for_retry(v),
                "legacy entry must be retry-eligible under {v}"
            );
        }
    }

    #[test]
    fn same_version_v06x_fixture_is_not_retry_eligible_under_same_binary() {
        let (_dir, state) = load_fixture(V06X_SAME_VERSION);
        let (_key, record) = state.iter().next().expect("one current entry");
        assert_eq!(record.cass_version_at_quarantine.as_deref(), Some("0.6.13"));
        // Same binary version: already tried — must NOT retry (avoid storms).
        assert!(!record.is_version_stale_for_retry("0.6.13"));
        // A later version may have fixed the bug — eligible again.
        assert!(record.is_version_stale_for_retry("0.6.14"));
    }

    #[test]
    fn old_version_fixture_is_retry_eligible_after_bump() {
        let (_dir, state) = load_fixture(OLD_VERSION);
        let (_key, record) = state.iter().next().expect("one old entry");
        assert_eq!(record.cass_version_at_quarantine.as_deref(), Some("0.5.1"));
        assert!(record.is_version_stale_for_retry("0.6.13"));
    }

    #[test]
    fn malformed_fixture_loads_as_empty_and_does_not_block_indexing() {
        let (_dir, state) = load_fixture(MALFORMED);
        assert!(
            state.is_empty(),
            "a malformed quarantine_state.json must degrade to empty, never error"
        );
    }

    #[test]
    fn missing_state_file_loads_as_empty() {
        // "source path missing" analog: no quarantine_state.json present.
        let dir = tempdir().expect("tempdir");
        let state = QuarantineState::load(dir.path());
        assert!(state.is_empty());
    }

    #[test]
    fn duplicate_collapsed_fixture_is_a_single_bounded_record() {
        let (dir, mut state) = load_fixture(DUPLICATE_COLLAPSED);
        assert_eq!(state.len(), 1, "many attempts collapse into one record");
        let key = QuarantineKey::new("conv-hot-poison", 1);
        {
            let record = state.entries.get(&key.storage_key()).expect("entry");
            assert_eq!(record.attempt_count, 5);
            assert!(
                record.first_attempt_at < record.last_attempt_at,
                "first_attempt_at preserved while last advances"
            );
        }
        // Recording another attempt on the same key must update in place,
        // never append — bounded growth under a hot poison session (#243).
        let now = DateTime::<Utc>::from_timestamp(1_716_000_000, 0).expect("valid timestamp");
        state.record_attempt(&key, "index-ingest-out-of-memory: 4.5 GiB", now);
        assert_eq!(state.len(), 1, "dedup-in-place: no unbounded growth");
        assert_eq!(
            state
                .entries
                .get(&key.storage_key())
                .expect("entry")
                .attempt_count,
            6
        );
        let _ = dir;
    }

    #[test]
    fn schema_version_bump_fixture_yields_distinct_keys() {
        let (_dir, state) = load_fixture(SCHEMA_VERSION_BUMP);
        assert_eq!(state.len(), 2, "a schema bump produces a fresh entry");
        let versions: Vec<u32> = state.iter().map(|(k, _)| k.schema_version).collect();
        assert_eq!(versions, vec![1, 2], "deterministic order by storage key");
        for (k, _) in state.iter() {
            assert_eq!(k.conversation_id, "conv-bumped");
        }
    }

    #[test]
    fn grouping_matrix_fixture_supports_status_grouping() {
        let (_dir, state) = load_fixture(GROUPING_MATRIX);
        assert_eq!(state.len(), 3);

        // Deterministic ordering by storage key: conv-a, conv-b, conv-c.
        let ids: Vec<String> = state.iter().map(|(k, _)| k.conversation_id).collect();
        assert_eq!(ids, vec!["conv-a", "conv-b", "conv-c"]);

        // Grouping by recorded cass version (status surfaces group by cause
        // version + eligibility). `None` = legacy carry-over bucket.
        let mut by_version: BTreeMap<Option<String>, usize> = BTreeMap::new();
        for (_k, record) in state.iter() {
            *by_version
                .entry(record.cass_version_at_quarantine.clone())
                .or_default() += 1;
        }
        assert_eq!(by_version.get(&Some("0.5.1".to_string())), Some(&1));
        assert_eq!(by_version.get(&Some("0.6.13".to_string())), Some(&1));
        assert_eq!(
            by_version.get(&None),
            Some(&1),
            "one legacy carry-over entry"
        );

        // Eligibility grouping under a current binary: the 0.5.1 and the
        // legacy (None) entries are retry-eligible; the 0.6.13 one is not.
        let eligible = state
            .iter()
            .filter(|(_, r)| r.is_version_stale_for_retry("0.6.13"))
            .count();
        assert_eq!(eligible, 2);
    }

    #[test]
    fn well_formed_fixtures_roundtrip_and_serialize_deterministically() {
        for (name, json) in WELL_FORMED {
            let (dir, state) = load_fixture(json);
            let before = state.len();

            // Save through the production atomic path, reload, and confirm
            // the entry set is preserved exactly.
            state
                .save(dir.path())
                .unwrap_or_else(|e| panic!("save {name}: {e}"));
            let reloaded = QuarantineState::load(dir.path());
            assert_eq!(reloaded.len(), before, "{name} entry count after roundtrip");
            assert_eq!(reloaded.entries, state.entries, "{name} entries preserved");

            // Determinism: serialising twice yields byte-identical JSON
            // (BTreeMap-backed stable ordering).
            let a = serde_json::to_string_pretty(&reloaded).expect("serialize");
            let b = serde_json::to_string_pretty(&reloaded).expect("serialize");
            assert_eq!(a, b, "{name} serialisation is deterministic");
        }
    }
}
