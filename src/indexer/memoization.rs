// Dead-code tolerated module-wide: the memoization vocabulary lands here
// ahead of the ConversationPacket-driven dataflow migration in ibuuh.32.
// Downstream slices will wire `ContentAddressedMemoCache` into the lexical
// normalization, token extraction, and semantic-embedding paths.
#![allow(dead_code)]

//! Content-addressed memoization vocabulary (bead ibuuh.34).
//!
//! The refresh pipeline recomputes the same derived artifacts over and
//! over for repeated content: historical salvage replays prior packets
//! verbatim, multi-session corpora repeat boilerplate tool banter, and
//! semantic rebuilds re-embed unchanged content after a version bump.
//! Content-addressed memoization lets those repeated inputs skip the
//! expensive derivation work without risking stale or cross-version
//! reuse.
//!
//! This module lands only the vocabulary: a key that combines a stable
//! content hash with an algorithm + version fingerprint, a bounded LRU
//! cache with structured audit logging for hit/miss/evict/quarantine,
//! and unit tests that pin the invariants. The actual wiring into the
//! rebuild pipeline ships in a follow-up slice once the
//! ConversationPacket contract (ibuuh.32) is migrated and the hot
//! derivations are factored through it.
//!
//! Invariants the types enforce:
//! - Memo keys always combine content hash AND `(algorithm,
//!   algorithm_version)`, so a version bump of any derivation
//!   automatically invalidates its prior cache entries — silent stale
//!   cross-version reuse is impossible by construction.
//! - Quarantined entries stay resident but are never served; the audit
//!   log records why quarantine happened so an operator can inspect.
//! - Evictions are driven only by a bounded entry budget. Callers pick
//!   the budget; no hidden global cache exists.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde::{Deserialize, Serialize};

/// Stable content fingerprint. The producer is responsible for
/// computing this from the canonical packet content; keeping it as
/// plain bytes here keeps this module independent of the hasher
/// choice (blake3 today, whatever frankensearch switches to tomorrow).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct MemoContentHash(pub Vec<u8>);

impl MemoContentHash {
    pub(crate) fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Composite memoization key: content hash + algorithm identity +
/// algorithm version fingerprint. A cache lookup hits only when ALL
/// three components match byte-for-byte, so a version bump (schema,
/// embedder, tokenizer, etc.) transparently invalidates every prior
/// entry whose algorithm fingerprint differs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct MemoKey {
    pub content_hash: MemoContentHash,
    pub algorithm: String,
    pub algorithm_version: String,
}

impl MemoKey {
    pub(crate) fn new(
        content_hash: MemoContentHash,
        algorithm: impl Into<String>,
        algorithm_version: impl Into<String>,
    ) -> Self {
        Self {
            content_hash,
            algorithm: algorithm.into(),
            algorithm_version: algorithm_version.into(),
        }
    }
}

/// Lookup outcome for a single cache query, surfaced both as a return
/// value and as a structured audit event so operators can reason about
/// cache behavior from logs alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub(crate) enum MemoLookup<V> {
    Hit { value: V },
    Miss,
    Quarantined { reason: String },
}

/// Event emitted for every mutating cache operation. Keeping the
/// vocabulary unified here means downstream structured logs are stable
/// across backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub(crate) enum MemoCacheEvent {
    Hit,
    Miss,
    Insert,
    Evict { reason: MemoEvictReason },
    Quarantine { reason: String },
    Invalidate,
}

/// Stable operation label carried alongside [`MemoCacheEvent`] in audit
/// records so downstream logs can distinguish a lookup that merely
/// observed quarantine from a producer-side mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MemoCacheOperation {
    Lookup,
    Insert,
    Invalidate,
    Quarantine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MemoEvictReason {
    /// Evicted because the cache reached `max_entries` and the entry
    /// was the least-recently-used.
    CapacityLru,
    /// Evicted because the producer called `invalidate_key`.
    Invalidated,
}

/// Running counters for an individual cache instance; serialized on
/// every mutating operation so tests and operators can pin behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MemoCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub inserts: u64,
    pub evictions_capacity: u64,
    pub invalidations: u64,
    pub quarantined: u64,
    pub live_entries: u64,
}

/// Structured operator-facing audit record for a single memo cache
/// decision. Wiring sites can serialize this directly into
/// refresh/rebuild traces once the packet dataflow is fully migrated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MemoCacheAuditRecord {
    pub operation: MemoCacheOperation,
    pub key: MemoKey,
    pub event: MemoCacheEvent,
    pub changed: bool,
    pub entry_capacity: usize,
    pub quarantined_entries: usize,
    pub stats: MemoCacheStats,
}

/// Stable operator-facing inspection row for a quarantined memo entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MemoQuarantineInspectionItem {
    pub key: MemoKey,
    pub reason: String,
}

/// Aggregated operator-facing quarantine counts. BTreeMap keeps JSON
/// output stable for robot consumers and lifecycle proofs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MemoQuarantineSummary {
    pub quarantined_entries: usize,
    pub reasons: BTreeMap<String, usize>,
    pub algorithms: BTreeMap<String, usize>,
}

/// Bounded in-memory content-addressed cache. Keyed on `MemoKey` and
/// driven by LRU eviction when `max_entries` is reached. Quarantined
/// entries stay resident (so an operator can inspect them) but never
/// serve a hit.
#[derive(Debug)]
pub(crate) struct ContentAddressedMemoCache<V: Clone> {
    max_entries: usize,
    entries: HashMap<MemoKey, V>,
    quarantined: HashMap<MemoKey, String>,
    lru: VecDeque<MemoKey>,
    stats: MemoCacheStats,
}

impl<V: Clone> ContentAddressedMemoCache<V> {
    pub(crate) fn with_capacity(max_entries: usize) -> Self {
        let cap = max_entries.max(1);
        Self {
            max_entries: cap,
            entries: HashMap::with_capacity(cap),
            quarantined: HashMap::new(),
            lru: VecDeque::with_capacity(cap),
            stats: MemoCacheStats::default(),
        }
    }

    pub(crate) fn get(&mut self, key: &MemoKey) -> MemoLookup<V> {
        self.get_with_audit(key).0
    }

    pub(crate) fn get_with_audit(
        &mut self,
        key: &MemoKey,
    ) -> (MemoLookup<V>, MemoCacheAuditRecord) {
        if let Some(reason) = self.quarantined.get(key) {
            let lookup = MemoLookup::Quarantined {
                reason: reason.clone(),
            };
            let audit = self.audit_record(
                MemoCacheOperation::Lookup,
                key.clone(),
                MemoCacheEvent::Quarantine {
                    reason: reason.clone(),
                },
                false,
            );
            return (lookup, audit);
        }
        match self.entries.get(key) {
            Some(value) => {
                let v = value.clone();
                self.touch(key);
                self.stats.hits = self.stats.hits.saturating_add(1);
                let lookup = MemoLookup::Hit { value: v };
                let audit = self.audit_record(
                    MemoCacheOperation::Lookup,
                    key.clone(),
                    MemoCacheEvent::Hit,
                    false,
                );
                (lookup, audit)
            }
            None => {
                self.stats.misses = self.stats.misses.saturating_add(1);
                let audit = self.audit_record(
                    MemoCacheOperation::Lookup,
                    key.clone(),
                    MemoCacheEvent::Miss,
                    false,
                );
                (MemoLookup::Miss, audit)
            }
        }
    }

    pub(crate) fn insert(&mut self, key: MemoKey, value: V) -> MemoCacheEvent {
        self.insert_with_audit(key, value).event
    }

    pub(crate) fn insert_with_audit(&mut self, key: MemoKey, value: V) -> MemoCacheAuditRecord {
        if self.quarantined.contains_key(&key) {
            // Insertion silently downgraded to noop: never overwrite a
            // quarantined entry. The caller should lift the quarantine
            // explicitly before re-inserting.
            let reason = self
                .quarantined
                .get(&key)
                .cloned()
                .unwrap_or_else(|| "quarantined".to_owned());
            return self.audit_record(
                MemoCacheOperation::Insert,
                key,
                MemoCacheEvent::Quarantine { reason },
                false,
            );
        }
        let mut evicted = false;
        if !self.entries.contains_key(&key)
            && self.entries.len() >= self.max_entries
            && let Some(victim) = self.lru.pop_front()
        {
            self.entries.remove(&victim);
            evicted = true;
        }
        // Re-insert OR fresh-insert both retain position at tail.
        self.lru.retain(|existing| existing != &key);
        self.lru.push_back(key.clone());
        self.entries.insert(key, value);
        self.stats.inserts = self.stats.inserts.saturating_add(1);
        self.stats.live_entries = self.entries.len() as u64;
        let event = if evicted {
            self.stats.evictions_capacity = self.stats.evictions_capacity.saturating_add(1);
            MemoCacheEvent::Evict {
                reason: MemoEvictReason::CapacityLru,
            }
        } else {
            MemoCacheEvent::Insert
        };
        let inserted_key = self
            .lru
            .back()
            .cloned()
            .unwrap_or_else(|| unreachable!("inserted key must exist in LRU"));
        self.audit_record(MemoCacheOperation::Insert, inserted_key, event, true)
    }

    pub(crate) fn invalidate(&mut self, key: &MemoKey) -> bool {
        self.invalidate_with_audit(key).changed
    }

    pub(crate) fn invalidate_with_audit(&mut self, key: &MemoKey) -> MemoCacheAuditRecord {
        let removed = self.entries.remove(key).is_some();
        self.lru.retain(|existing| existing != key);
        if removed {
            self.stats.invalidations = self.stats.invalidations.saturating_add(1);
            self.stats.live_entries = self.entries.len() as u64;
        }
        self.audit_record(
            MemoCacheOperation::Invalidate,
            key.clone(),
            MemoCacheEvent::Invalidate,
            removed,
        )
    }

    pub(crate) fn quarantine(&mut self, key: MemoKey, reason: impl Into<String>) {
        let _ = self.quarantine_with_audit(key, reason);
    }

    pub(crate) fn quarantine_with_audit(
        &mut self,
        key: MemoKey,
        reason: impl Into<String>,
    ) -> MemoCacheAuditRecord {
        let reason = reason.into();
        let previous_reason = self.quarantined.get(&key).cloned();
        let had_entry = self.entries.contains_key(&key);
        self.entries.remove(&key);
        self.lru.retain(|existing| existing != &key);
        let newly_quarantined = !self.quarantined.contains_key(&key);
        self.quarantined.insert(key.clone(), reason.clone());
        if newly_quarantined {
            self.stats.quarantined = self.stats.quarantined.saturating_add(1);
        }
        self.stats.live_entries = self.entries.len() as u64;
        let changed = had_entry || previous_reason.as_deref() != Some(reason.as_str());
        self.audit_record(
            MemoCacheOperation::Quarantine,
            key,
            MemoCacheEvent::Quarantine { reason },
            changed,
        )
    }

    pub(crate) fn stats(&self) -> &MemoCacheStats {
        &self.stats
    }

    pub(crate) fn quarantine_inspection_items(&self) -> Vec<MemoQuarantineInspectionItem> {
        let mut items: Vec<_> = self
            .quarantined
            .iter()
            .map(|(key, reason)| MemoQuarantineInspectionItem {
                key: key.clone(),
                reason: reason.clone(),
            })
            .collect();
        sort_quarantine_inspection_items(&mut items);
        items
    }

    pub(crate) fn quarantine_summary(&self) -> MemoQuarantineSummary {
        let mut summary = MemoQuarantineSummary {
            quarantined_entries: self.quarantined.len(),
            ..MemoQuarantineSummary::default()
        };
        for (key, reason) in &self.quarantined {
            *summary.reasons.entry(reason.clone()).or_insert(0) += 1;
            *summary.algorithms.entry(key.algorithm.clone()).or_insert(0) += 1;
        }
        summary
    }

    /// Remove an inspected quarantine tombstone and return its audit
    /// record. This is in-memory metadata GC only; persisted artifact
    /// retention stays with the caller.
    pub(crate) fn garbage_collect_quarantined(
        &mut self,
        key: &MemoKey,
    ) -> Option<MemoQuarantineInspectionItem> {
        self.quarantined
            .remove(key)
            .map(|reason| MemoQuarantineInspectionItem {
                key: key.clone(),
                reason,
            })
    }

    /// Preview which quarantine tombstones would be collected for an
    /// algorithm without mutating the cache.
    pub(crate) fn preview_garbage_collect_quarantined_algorithm(
        &self,
        algorithm: &str,
    ) -> Vec<MemoQuarantineInspectionItem> {
        let mut items: Vec<_> = self
            .quarantined
            .iter()
            .filter(|(key, _)| key.algorithm == algorithm)
            .map(|(key, reason)| MemoQuarantineInspectionItem {
                key: key.clone(),
                reason: reason.clone(),
            })
            .collect();
        sort_quarantine_inspection_items(&mut items);
        items
    }

    /// Preview which quarantine tombstones would be collected for an
    /// exact reason string without mutating the cache.
    pub(crate) fn preview_garbage_collect_quarantined_reason(
        &self,
        reason: &str,
    ) -> Vec<MemoQuarantineInspectionItem> {
        let mut items: Vec<_> = self
            .quarantined
            .iter()
            .filter(|(_, stored_reason)| stored_reason.as_str() == reason)
            .map(|(key, stored_reason)| MemoQuarantineInspectionItem {
                key: key.clone(),
                reason: stored_reason.clone(),
            })
            .collect();
        sort_quarantine_inspection_items(&mut items);
        items
    }

    /// Remove every inspected quarantine tombstone for an algorithm and
    /// return a deterministic audit list of what was collected.
    pub(crate) fn garbage_collect_quarantined_algorithm(
        &mut self,
        algorithm: &str,
    ) -> Vec<MemoQuarantineInspectionItem> {
        let keys: Vec<_> = self
            .quarantined
            .keys()
            .filter(|key| key.algorithm == algorithm)
            .cloned()
            .collect();
        let mut collected: Vec<_> = keys
            .into_iter()
            .filter_map(|key| self.garbage_collect_quarantined(&key))
            .collect();
        sort_quarantine_inspection_items(&mut collected);
        collected
    }

    /// Remove every inspected quarantine tombstone with an exact reason
    /// and return a deterministic audit list of what was collected.
    pub(crate) fn garbage_collect_quarantined_reason(
        &mut self,
        reason: &str,
    ) -> Vec<MemoQuarantineInspectionItem> {
        let preview = self.preview_garbage_collect_quarantined_reason(reason);
        for item in &preview {
            self.quarantined.remove(&item.key);
        }
        preview
    }

    fn touch(&mut self, key: &MemoKey) {
        if let Some(pos) = self.lru.iter().position(|existing| existing == key) {
            self.lru.remove(pos);
            self.lru.push_back(key.clone());
        }
    }

    fn audit_record(
        &self,
        operation: MemoCacheOperation,
        key: MemoKey,
        event: MemoCacheEvent,
        changed: bool,
    ) -> MemoCacheAuditRecord {
        MemoCacheAuditRecord {
            operation,
            key,
            event,
            changed,
            entry_capacity: self.max_entries,
            quarantined_entries: self.quarantined.len(),
            stats: self.stats.clone(),
        }
    }
}

fn sort_quarantine_inspection_items(items: &mut [MemoQuarantineInspectionItem]) {
    items.sort_by(|left, right| {
        left.key
            .algorithm
            .cmp(&right.key.algorithm)
            .then_with(|| left.key.algorithm_version.cmp(&right.key.algorithm_version))
            .then_with(|| {
                left.key
                    .content_hash
                    .as_bytes()
                    .cmp(right.key.content_hash.as_bytes())
            })
            .then_with(|| left.reason.cmp(&right.reason))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn key(content: &[u8], algo: &str, version: &str) -> MemoKey {
        MemoKey::new(MemoContentHash::from_bytes(content.to_vec()), algo, version)
    }

    fn memo_key_strategy() -> impl Strategy<Value = MemoKey> {
        (
            proptest::collection::vec(any::<u8>(), 0..65),
            ".{0,64}",
            ".{0,64}",
        )
            .prop_map(|(content_hash, algorithm, algorithm_version)| {
                MemoKey::new(
                    MemoContentHash::from_bytes(content_hash),
                    algorithm,
                    algorithm_version,
                )
            })
    }

    fn memo_event_strategy() -> impl Strategy<Value = MemoCacheEvent> {
        prop_oneof![
            Just(MemoCacheEvent::Hit),
            Just(MemoCacheEvent::Miss),
            Just(MemoCacheEvent::Insert),
            Just(MemoCacheEvent::Evict {
                reason: MemoEvictReason::CapacityLru,
            }),
            Just(MemoCacheEvent::Evict {
                reason: MemoEvictReason::Invalidated,
            }),
            ".{0,96}".prop_map(|reason| MemoCacheEvent::Quarantine { reason }),
            Just(MemoCacheEvent::Invalidate),
        ]
    }

    fn memo_operation_strategy() -> impl Strategy<Value = MemoCacheOperation> {
        prop_oneof![
            Just(MemoCacheOperation::Lookup),
            Just(MemoCacheOperation::Insert),
            Just(MemoCacheOperation::Invalidate),
            Just(MemoCacheOperation::Quarantine),
        ]
    }

    fn memo_lookup_strategy() -> impl Strategy<Value = MemoLookup<String>> {
        prop_oneof![
            ".{0,128}".prop_map(|value| MemoLookup::Hit { value }),
            Just(MemoLookup::Miss),
            ".{0,96}".prop_map(|reason| MemoLookup::Quarantined { reason }),
        ]
    }

    fn memo_stats_strategy() -> impl Strategy<Value = MemoCacheStats> {
        (
            0u64..10_000,
            0u64..10_000,
            0u64..10_000,
            0u64..10_000,
            0u64..10_000,
            0u64..10_000,
            0u64..10_000,
        )
            .prop_map(
                |(
                    hits,
                    misses,
                    inserts,
                    evictions_capacity,
                    invalidations,
                    quarantined,
                    live_entries,
                )| MemoCacheStats {
                    hits,
                    misses,
                    inserts,
                    evictions_capacity,
                    invalidations,
                    quarantined,
                    live_entries,
                },
            )
    }

    fn quarantine_item_strategy() -> impl Strategy<Value = MemoQuarantineInspectionItem> {
        (memo_key_strategy(), ".{0,96}")
            .prop_map(|(key, reason)| MemoQuarantineInspectionItem { key, reason })
    }

    fn quarantine_summary_strategy() -> impl Strategy<Value = MemoQuarantineSummary> {
        (
            0usize..1_000_000,
            proptest::collection::btree_map(".{0,32}", 0usize..1_000, 0..16),
            proptest::collection::btree_map(".{0,32}", 0usize..1_000, 0..16),
        )
            .prop_map(
                |(quarantined_entries, reasons, algorithms)| MemoQuarantineSummary {
                    quarantined_entries,
                    reasons,
                    algorithms,
                },
            )
    }

    fn memo_audit_record_strategy() -> impl Strategy<Value = MemoCacheAuditRecord> {
        (
            memo_operation_strategy(),
            memo_key_strategy(),
            memo_event_strategy(),
            any::<bool>(),
            1usize..256,
            0usize..256,
            memo_stats_strategy(),
        )
            .prop_map(
                |(operation, key, event, changed, entry_capacity, quarantined_entries, stats)| {
                    MemoCacheAuditRecord {
                        operation,
                        key,
                        event,
                        changed,
                        entry_capacity,
                        quarantined_entries,
                        stats,
                    }
                },
            )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn memo_key_json_round_trips_for_random_payloads(k in memo_key_strategy()) {
            let bytes = serde_json::to_vec(&k)?;
            let parsed: MemoKey = serde_json::from_slice(&bytes)?;
            prop_assert_eq!(parsed, k);
        }

        #[test]
        fn memo_lookup_json_round_trips_for_random_payloads(lookup in memo_lookup_strategy()) {
            let bytes = serde_json::to_vec(&lookup)?;
            let parsed: MemoLookup<String> = serde_json::from_slice(&bytes)?;
            prop_assert_eq!(parsed, lookup);
        }

        #[test]
        fn memo_cache_event_json_round_trips_for_random_payloads(event in memo_event_strategy()) {
            let bytes = serde_json::to_vec(&event)?;
            let parsed: MemoCacheEvent = serde_json::from_slice(&bytes)?;
            prop_assert_eq!(parsed, event);
        }

        #[test]
        fn memo_quarantine_json_round_trips_for_random_payloads(
            item in quarantine_item_strategy(),
            summary in quarantine_summary_strategy(),
        ) {
            let item_bytes = serde_json::to_vec(&item)?;
            let parsed_item: MemoQuarantineInspectionItem = serde_json::from_slice(&item_bytes)?;
            prop_assert_eq!(parsed_item, item);

            let summary_bytes = serde_json::to_vec(&summary)?;
            let parsed_summary: MemoQuarantineSummary = serde_json::from_slice(&summary_bytes)?;
            prop_assert_eq!(parsed_summary, summary);
        }

        #[test]
        fn memo_audit_record_json_round_trips_for_random_payloads(
            record in memo_audit_record_strategy(),
        ) {
            let bytes = serde_json::to_vec(&record)?;
            let parsed: MemoCacheAuditRecord = serde_json::from_slice(&bytes)?;
            prop_assert_eq!(parsed, record);
        }
    }

    #[test]
    fn memo_key_distinguishes_by_content_algorithm_and_version() {
        let base = key(b"abc", "lex", "v1");
        assert_eq!(base, key(b"abc", "lex", "v1"));
        assert_ne!(base, key(b"abd", "lex", "v1"), "content hash mismatch");
        assert_ne!(base, key(b"abc", "tok", "v1"), "algorithm mismatch");
        assert_ne!(base, key(b"abc", "lex", "v2"), "version mismatch");
    }

    #[test]
    fn memo_key_round_trips_through_json() {
        let k = key(b"hello", "lex", "v1");
        let bytes = serde_json::to_vec(&k).unwrap();
        let parsed: MemoKey = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, k);
    }

    #[test]
    fn empty_cache_returns_miss_and_records_stat() -> Result<(), String> {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(4);
        let k = key(b"missing", "lex", "v1");
        match cache.get(&k) {
            MemoLookup::Miss => {}
            other => return Err(format!("expected Miss, got {other:?}")),
        }
        assert_eq!(cache.stats().misses, 1);
        assert_eq!(cache.stats().hits, 0);
        Ok(())
    }

    #[test]
    fn insert_then_get_returns_hit_and_bumps_counters() -> Result<(), String> {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(4);
        let k = key(b"x", "lex", "v1");
        let event = cache.insert(k.clone(), "derived".into());
        assert_eq!(event, MemoCacheEvent::Insert);
        match cache.get(&k) {
            MemoLookup::Hit { value } => assert_eq!(value, "derived"),
            other => return Err(format!("expected Hit, got {other:?}")),
        }
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.inserts, 1);
        assert_eq!(stats.live_entries, 1);
        Ok(())
    }

    #[test]
    fn version_bump_does_not_hit_prior_entry() -> Result<(), String> {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(4);
        cache.insert(key(b"x", "lex", "v1"), "old".into());
        // Same content + algorithm, new version fingerprint.
        match cache.get(&key(b"x", "lex", "v2")) {
            MemoLookup::Miss => {}
            other => return Err(format!("version bump must miss prior cache; got {other:?}")),
        }
        Ok(())
    }

    #[test]
    fn capacity_lru_evicts_oldest_and_reports_event() {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(2);
        cache.insert(key(b"a", "lex", "v1"), "A".into());
        cache.insert(key(b"b", "lex", "v1"), "B".into());
        // Touch A to make B the LRU victim.
        let _ = cache.get(&key(b"a", "lex", "v1"));
        let event = cache.insert(key(b"c", "lex", "v1"), "C".into());
        assert_eq!(
            event,
            MemoCacheEvent::Evict {
                reason: MemoEvictReason::CapacityLru
            }
        );
        // A and C must survive, B must be gone.
        assert!(matches!(
            cache.get(&key(b"a", "lex", "v1")),
            MemoLookup::Hit { .. }
        ));
        assert!(matches!(
            cache.get(&key(b"c", "lex", "v1")),
            MemoLookup::Hit { .. }
        ));
        assert!(matches!(
            cache.get(&key(b"b", "lex", "v1")),
            MemoLookup::Miss
        ));
        assert_eq!(cache.stats().evictions_capacity, 1);
    }

    #[test]
    fn invalidate_removes_entry_and_bumps_counter() {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(4);
        let k = key(b"x", "lex", "v1");
        cache.insert(k.clone(), "value".into());
        assert!(cache.invalidate(&k));
        assert_eq!(cache.stats().invalidations, 1);
        assert!(matches!(cache.get(&k), MemoLookup::Miss));
        // Invalidating a missing key returns false without bumping the
        // counter.
        assert!(!cache.invalidate(&k));
        assert_eq!(cache.stats().invalidations, 1);
    }

    #[test]
    fn quarantined_entry_stays_resident_but_never_hits() -> Result<(), String> {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(4);
        let k = key(b"x", "lex", "v1");
        cache.insert(k.clone(), "value".into());
        cache.quarantine(k.clone(), "suspected-corruption");
        match cache.get(&k) {
            MemoLookup::Quarantined { reason } => assert_eq!(reason, "suspected-corruption"),
            other => return Err(format!("expected Quarantined, got {other:?}")),
        }
        // Re-inserting a quarantined key must NOT silently overwrite.
        let event = cache.insert(k.clone(), "replacement".into());
        assert!(matches!(event, MemoCacheEvent::Quarantine { .. }));
        match cache.get(&k) {
            MemoLookup::Quarantined { .. } => {}
            other => return Err(format!("quarantine must persist; got {other:?}")),
        }
        assert_eq!(cache.stats().quarantined, 1);
        Ok(())
    }

    #[test]
    fn quarantine_inspection_items_are_stable_and_reasoned() {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(4);
        let semantic = key(b"semantic", "semantic-embed", "v2");
        let lexical_b = key(b"lex-b", "lexical-normalize", "v1");
        let lexical_a = key(b"lex-a", "lexical-normalize", "v1");

        cache.insert(semantic.clone(), "semantic-value".into());
        cache.insert(lexical_b.clone(), "lexical-b".into());
        cache.insert(lexical_a.clone(), "lexical-a".into());
        cache.quarantine(semantic, "embedding checksum mismatch");
        cache.quarantine(lexical_b, "normalizer panic replay");
        cache.quarantine(lexical_a, "invalid unicode boundary");

        let items = cache.quarantine_inspection_items();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].key, key(b"lex-a", "lexical-normalize", "v1"));
        assert_eq!(items[0].reason, "invalid unicode boundary");
        assert_eq!(items[1].key, key(b"lex-b", "lexical-normalize", "v1"));
        assert_eq!(items[1].reason, "normalizer panic replay");
        assert_eq!(items[2].key, key(b"semantic", "semantic-embed", "v2"));
        assert_eq!(items[2].reason, "embedding checksum mismatch");

        let json = serde_json::to_value(&items).expect("serialize quarantine inspection items");
        assert_eq!(json[0]["key"]["algorithm"], "lexical-normalize");
        assert_eq!(json[0]["reason"], "invalid unicode boundary");
        assert_eq!(json[2]["key"]["algorithm"], "semantic-embed");
    }

    #[test]
    fn quarantine_summary_groups_by_reason_and_algorithm() -> Result<(), serde_json::Error> {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(4);
        let lexical_a = key(b"lex-a", "lexical-normalize", "v1");
        let lexical_b = key(b"lex-b", "lexical-normalize", "v1");
        let semantic = key(b"semantic", "semantic-embed", "v2");

        cache.insert(lexical_a.clone(), "lexical-a".into());
        cache.insert(lexical_b.clone(), "lexical-b".into());
        cache.insert(semantic.clone(), "semantic".into());
        cache.quarantine(lexical_a, "checksum mismatch");
        cache.quarantine(lexical_b, "checksum mismatch");
        cache.quarantine(semantic, "schema drift");

        let summary = cache.quarantine_summary();
        assert_eq!(summary.quarantined_entries, 3);
        assert_eq!(summary.reasons.get("checksum mismatch"), Some(&2));
        assert_eq!(summary.reasons.get("schema drift"), Some(&1));
        assert_eq!(summary.algorithms.get("lexical-normalize"), Some(&2));
        assert_eq!(summary.algorithms.get("semantic-embed"), Some(&1));

        let json = serde_json::to_value(&summary)?;
        assert_eq!(json["quarantined_entries"], 3);
        assert_eq!(json["reasons"]["checksum mismatch"], 2);
        assert_eq!(json["algorithms"]["semantic-embed"], 1);
        Ok(())
    }

    #[test]
    fn garbage_collect_quarantined_entry_after_inspection() -> Result<(), String> {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(4);
        let k = key(b"semantic", "semantic-embed", "v2");

        cache.insert(k.clone(), "old-vector".into());
        cache.quarantine(k.clone(), "checksum mismatch");
        let removed = cache
            .garbage_collect_quarantined(&k)
            .ok_or_else(|| "expected quarantined tombstone to be collected".to_string())?;
        assert_eq!(removed.key, k);
        assert_eq!(removed.reason, "checksum mismatch");
        assert_eq!(cache.quarantine_summary().quarantined_entries, 0);

        match cache.get(&removed.key) {
            MemoLookup::Miss => {}
            other => {
                return Err(format!(
                    "collected quarantine should expose a cache miss, got {other:?}"
                ));
            }
        }
        assert_eq!(
            cache.insert(removed.key.clone(), "replacement-vector".into()),
            MemoCacheEvent::Insert
        );
        match cache.get(&removed.key) {
            MemoLookup::Hit { value } => assert_eq!(value, "replacement-vector"),
            other => return Err(format!("expected reinsertion hit, got {other:?}")),
        }
        Ok(())
    }

    #[test]
    fn preview_garbage_collect_quarantined_algorithm_is_deterministic_and_non_mutating() {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(6);
        let lexical_b = key(b"lex-b", "lexical-normalize", "v1");
        let lexical_a = key(b"lex-a", "lexical-normalize", "v1");
        let semantic = key(b"semantic", "semantic-embed", "v2");

        cache.insert(lexical_b.clone(), "lexical-b".into());
        cache.insert(semantic.clone(), "semantic".into());
        cache.insert(lexical_a.clone(), "lexical-a".into());
        cache.quarantine(lexical_b, "normalizer panic replay");
        cache.quarantine(semantic.clone(), "embedding checksum mismatch");
        cache.quarantine(lexical_a, "invalid unicode boundary");

        let preview = cache.preview_garbage_collect_quarantined_algorithm("lexical-normalize");
        assert_eq!(preview.len(), 2);
        assert_eq!(preview[0].key, key(b"lex-a", "lexical-normalize", "v1"));
        assert_eq!(preview[0].reason, "invalid unicode boundary");
        assert_eq!(preview[1].key, key(b"lex-b", "lexical-normalize", "v1"));
        assert_eq!(preview[1].reason, "normalizer panic replay");

        let summary = cache.quarantine_summary();
        assert_eq!(summary.quarantined_entries, 3);
        assert_eq!(summary.algorithms.get("lexical-normalize"), Some(&2));
        assert!(matches!(
            cache.get(&semantic),
            MemoLookup::Quarantined { .. }
        ));
        assert!(
            cache
                .preview_garbage_collect_quarantined_algorithm("unknown-algorithm")
                .is_empty()
        );
    }

    #[test]
    fn garbage_collect_quarantined_algorithm_returns_stable_audit_items() {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(6);
        let lexical_b = key(b"lex-b", "lexical-normalize", "v1");
        let lexical_a = key(b"lex-a", "lexical-normalize", "v1");
        let semantic = key(b"semantic", "semantic-embed", "v2");

        cache.insert(lexical_b.clone(), "lexical-b".into());
        cache.insert(semantic.clone(), "semantic".into());
        cache.insert(lexical_a.clone(), "lexical-a".into());
        cache.quarantine(lexical_b, "normalizer panic replay");
        cache.quarantine(semantic.clone(), "embedding checksum mismatch");
        cache.quarantine(lexical_a, "invalid unicode boundary");

        let removed = cache.garbage_collect_quarantined_algorithm("lexical-normalize");
        assert_eq!(removed.len(), 2);
        assert_eq!(removed[0].key, key(b"lex-a", "lexical-normalize", "v1"));
        assert_eq!(removed[0].reason, "invalid unicode boundary");
        assert_eq!(removed[1].key, key(b"lex-b", "lexical-normalize", "v1"));
        assert_eq!(removed[1].reason, "normalizer panic replay");
        let summary = cache.quarantine_summary();
        assert_eq!(summary.quarantined_entries, 1);
        assert_eq!(summary.algorithms.get("semantic-embed"), Some(&1));
        assert_eq!(summary.algorithms.get("lexical-normalize"), None);
        assert!(matches!(
            cache.get(&semantic),
            MemoLookup::Quarantined { .. }
        ));
        assert!(
            cache
                .garbage_collect_quarantined_algorithm("lexical-normalize")
                .is_empty()
        );
    }

    #[test]
    fn garbage_collect_quarantined_reason_is_previewable_and_exact() {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(6);
        let lexical_a = key(b"lex-a", "lexical-normalize", "v1");
        let lexical_b = key(b"lex-b", "lexical-normalize", "v1");
        let semantic = key(b"semantic", "semantic-embed", "v2");

        cache.insert(lexical_a.clone(), "lexical-a".into());
        cache.insert(semantic.clone(), "semantic".into());
        cache.insert(lexical_b.clone(), "lexical-b".into());
        cache.quarantine(lexical_a, "checksum mismatch");
        cache.quarantine(semantic.clone(), "checksum mismatch");
        cache.quarantine(lexical_b.clone(), "checksum mismatch - retriable");

        let preview = cache.preview_garbage_collect_quarantined_reason("checksum mismatch");
        assert_eq!(preview.len(), 2);
        assert_eq!(preview[0].key, key(b"lex-a", "lexical-normalize", "v1"));
        assert_eq!(preview[0].reason, "checksum mismatch");
        assert_eq!(preview[1].key, key(b"semantic", "semantic-embed", "v2"));
        assert_eq!(preview[1].reason, "checksum mismatch");
        assert_eq!(cache.quarantine_summary().quarantined_entries, 3);

        let collected = cache.garbage_collect_quarantined_reason("checksum mismatch");
        assert_eq!(collected, preview);
        let summary = cache.quarantine_summary();
        assert_eq!(summary.quarantined_entries, 1);
        assert_eq!(summary.reasons.get("checksum mismatch"), None);
        assert_eq!(
            summary.reasons.get("checksum mismatch - retriable"),
            Some(&1)
        );
        assert!(matches!(
            cache.get(&lexical_b),
            MemoLookup::Quarantined { .. }
        ));
        assert!(
            cache
                .garbage_collect_quarantined_reason("checksum mismatch")
                .is_empty()
        );
    }

    #[test]
    fn stats_serialize_as_snake_case_and_count_live_entries() {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(2);
        cache.insert(key(b"a", "lex", "v1"), "A".into());
        cache.insert(key(b"b", "lex", "v1"), "B".into());
        let stats = cache.stats().clone();
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("\"live_entries\":2"));
        assert!(json.contains("\"inserts\":2"));
        assert!(json.contains("\"hits\":0"));
    }

    #[test]
    fn get_with_audit_reports_lookup_event_and_capacity() -> Result<(), String> {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(2);
        let key = key(b"hit", "lexical-normalize", "v1");
        cache.insert(key.clone(), "derived".into());

        let (lookup, audit) = cache.get_with_audit(&key);
        match lookup {
            MemoLookup::Hit { value } => assert_eq!(value, "derived"),
            other => return Err(format!("expected hit lookup, got {other:?}")),
        }
        assert_eq!(audit.operation, MemoCacheOperation::Lookup);
        assert_eq!(audit.key, key);
        assert_eq!(audit.event, MemoCacheEvent::Hit);
        assert!(!audit.changed);
        assert_eq!(audit.entry_capacity, 2);
        assert_eq!(audit.quarantined_entries, 0);
        assert_eq!(audit.stats.hits, 1);
        assert_eq!(audit.stats.live_entries, 1);
        Ok(())
    }

    #[test]
    fn insert_with_audit_reports_quarantine_noop_and_json_shape() -> Result<(), String> {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(2);
        let key = key(b"blocked", "semantic-embed", "v2");
        cache.quarantine(key.clone(), "checksum mismatch");

        let audit = cache.insert_with_audit(key.clone(), "replacement".into());
        assert_eq!(audit.operation, MemoCacheOperation::Insert);
        assert_eq!(audit.key, key);
        assert_eq!(
            audit.event,
            MemoCacheEvent::Quarantine {
                reason: "checksum mismatch".to_string()
            }
        );
        assert!(!audit.changed);
        assert_eq!(audit.quarantined_entries, 1);
        let json = serde_json::to_value(&audit).map_err(|err| err.to_string())?;
        assert_eq!(json["operation"], "insert");
        assert_eq!(json["event"]["kind"], "quarantine");
        assert_eq!(json["event"]["reason"], "checksum mismatch");
        assert_eq!(json["changed"], false);
        Ok(())
    }

    #[test]
    fn invalidate_with_audit_reports_removed_state() {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(2);
        let present = key(b"present", "lex", "v1");
        let missing = key(b"missing", "lex", "v1");
        cache.insert(present.clone(), "value".into());

        let removed = cache.invalidate_with_audit(&present);
        assert_eq!(removed.operation, MemoCacheOperation::Invalidate);
        assert_eq!(removed.key, present);
        assert_eq!(removed.event, MemoCacheEvent::Invalidate);
        assert!(removed.changed);
        assert_eq!(removed.stats.invalidations, 1);
        assert_eq!(removed.stats.live_entries, 0);

        let noop = cache.invalidate_with_audit(&missing);
        assert_eq!(noop.operation, MemoCacheOperation::Invalidate);
        assert_eq!(noop.key, missing);
        assert_eq!(noop.event, MemoCacheEvent::Invalidate);
        assert!(!noop.changed);
        assert_eq!(noop.stats.invalidations, 1);
        assert_eq!(noop.stats.live_entries, 0);
    }

    #[test]
    fn quarantine_with_audit_reports_current_quarantine_state() -> Result<(), String> {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(3);
        let key = key(b"memo", "lexical-normalize", "v1");
        cache.insert(key.clone(), "derived".into());

        let audit = cache.quarantine_with_audit(key.clone(), "normalizer panic replay");
        assert_eq!(audit.operation, MemoCacheOperation::Quarantine);
        assert_eq!(audit.key, key);
        assert_eq!(
            audit.event,
            MemoCacheEvent::Quarantine {
                reason: "normalizer panic replay".to_string()
            }
        );
        assert!(audit.changed);
        assert_eq!(audit.quarantined_entries, 1);
        assert_eq!(audit.stats.quarantined, 1);
        assert_eq!(audit.stats.live_entries, 0);
        let json = serde_json::to_string(&audit).map_err(|err| err.to_string())?;
        assert!(json.contains("\"operation\":\"quarantine\""));
        assert!(json.contains("\"entry_capacity\":3"));
        Ok(())
    }
}
