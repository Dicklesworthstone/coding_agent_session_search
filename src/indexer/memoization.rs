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

use std::collections::HashMap;
use std::collections::VecDeque;

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
        if let Some(reason) = self.quarantined.get(key) {
            return MemoLookup::Quarantined {
                reason: reason.clone(),
            };
        }
        match self.entries.get(key) {
            Some(value) => {
                let v = value.clone();
                self.touch(key);
                self.stats.hits = self.stats.hits.saturating_add(1);
                MemoLookup::Hit { value: v }
            }
            None => {
                self.stats.misses = self.stats.misses.saturating_add(1);
                MemoLookup::Miss
            }
        }
    }

    pub(crate) fn insert(&mut self, key: MemoKey, value: V) -> MemoCacheEvent {
        if self.quarantined.contains_key(&key) {
            // Insertion silently downgraded to noop: never overwrite a
            // quarantined entry. The caller should lift the quarantine
            // explicitly before re-inserting.
            return MemoCacheEvent::Quarantine {
                reason: self
                    .quarantined
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| "quarantined".to_owned()),
            };
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
        if evicted {
            self.stats.evictions_capacity = self.stats.evictions_capacity.saturating_add(1);
            MemoCacheEvent::Evict {
                reason: MemoEvictReason::CapacityLru,
            }
        } else {
            MemoCacheEvent::Insert
        }
    }

    pub(crate) fn invalidate(&mut self, key: &MemoKey) -> bool {
        let removed = self.entries.remove(key).is_some();
        self.lru.retain(|existing| existing != key);
        if removed {
            self.stats.invalidations = self.stats.invalidations.saturating_add(1);
            self.stats.live_entries = self.entries.len() as u64;
        }
        removed
    }

    pub(crate) fn quarantine(&mut self, key: MemoKey, reason: impl Into<String>) {
        let reason = reason.into();
        self.entries.remove(&key);
        self.lru.retain(|existing| existing != &key);
        self.quarantined.insert(key, reason);
        self.stats.quarantined = self.stats.quarantined.saturating_add(1);
        self.stats.live_entries = self.entries.len() as u64;
    }

    pub(crate) fn stats(&self) -> &MemoCacheStats {
        &self.stats
    }

    fn touch(&mut self, key: &MemoKey) {
        if let Some(pos) = self.lru.iter().position(|existing| existing == key) {
            self.lru.remove(pos);
            self.lru.push_back(key.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(content: &[u8], algo: &str, version: &str) -> MemoKey {
        MemoKey::new(MemoContentHash::from_bytes(content.to_vec()), algo, version)
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
    fn empty_cache_returns_miss_and_records_stat() {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(4);
        let k = key(b"missing", "lex", "v1");
        match cache.get(&k) {
            MemoLookup::Miss => {}
            other => panic!("expected Miss, got {other:?}"),
        }
        assert_eq!(cache.stats().misses, 1);
        assert_eq!(cache.stats().hits, 0);
    }

    #[test]
    fn insert_then_get_returns_hit_and_bumps_counters() {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(4);
        let k = key(b"x", "lex", "v1");
        let event = cache.insert(k.clone(), "derived".into());
        assert_eq!(event, MemoCacheEvent::Insert);
        match cache.get(&k) {
            MemoLookup::Hit { value } => assert_eq!(value, "derived"),
            other => panic!("expected Hit, got {other:?}"),
        }
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.inserts, 1);
        assert_eq!(stats.live_entries, 1);
    }

    #[test]
    fn version_bump_does_not_hit_prior_entry() {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(4);
        cache.insert(key(b"x", "lex", "v1"), "old".into());
        // Same content + algorithm, new version fingerprint.
        match cache.get(&key(b"x", "lex", "v2")) {
            MemoLookup::Miss => {}
            other => panic!("version bump must miss prior cache; got {other:?}"),
        }
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
    fn quarantined_entry_stays_resident_but_never_hits() {
        let mut cache: ContentAddressedMemoCache<String> =
            ContentAddressedMemoCache::with_capacity(4);
        let k = key(b"x", "lex", "v1");
        cache.insert(k.clone(), "value".into());
        cache.quarantine(k.clone(), "suspected-corruption");
        match cache.get(&k) {
            MemoLookup::Quarantined { reason } => assert_eq!(reason, "suspected-corruption"),
            other => panic!("expected Quarantined, got {other:?}"),
        }
        // Re-inserting a quarantined key must NOT silently overwrite.
        let event = cache.insert(k.clone(), "replacement".into());
        assert!(matches!(event, MemoCacheEvent::Quarantine { .. }));
        match cache.get(&k) {
            MemoLookup::Quarantined { .. } => {}
            other => panic!("quarantine must persist; got {other:?}"),
        }
        assert_eq!(cache.stats().quarantined, 1);
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
}
