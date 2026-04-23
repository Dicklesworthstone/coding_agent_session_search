#[path = "../src/indexer/memoization.rs"]
mod memoization;

use memoization::{ContentAddressedMemoCache, MemoContentHash, MemoKey, MemoLookup};

fn key(label: &str) -> MemoKey {
    MemoKey::new(
        MemoContentHash::from_bytes(label.as_bytes().to_vec()),
        "semantic_prepare_window",
        "v1",
    )
}

#[test]
fn bounded_capacity_policy_evicts_cold_entries_under_sustained_load() -> Result<(), String> {
    const CAPACITY: usize = 16;
    const HOT_KEYS: usize = 4;
    const COLD_INSERTS: usize = 64;

    let mut cache: ContentAddressedMemoCache<String> =
        ContentAddressedMemoCache::with_capacity(CAPACITY);
    let hot_keys: Vec<MemoKey> = (0..HOT_KEYS).map(|i| key(&format!("hot-{i}"))).collect();

    for (i, hot_key) in hot_keys.iter().enumerate() {
        let _ = cache.insert(hot_key.clone(), format!("hot-{i}"));
    }

    let mut max_live_entries = cache.stats().live_entries as usize;
    for round in 0..COLD_INSERTS {
        for hot_key in &hot_keys {
            assert!(
                matches!(cache.get(hot_key), MemoLookup::Hit { .. }),
                "hot working-set entry {hot_key:?} should stay resident"
            );
        }

        let cold_key = key(&format!("cold-{round}"));
        let _ = cache.insert(cold_key.clone(), format!("cold-{round}"));
        assert!(
            matches!(cache.get(&cold_key), MemoLookup::Hit { .. }),
            "fresh entry cold-{round} should still be present immediately after insert"
        );

        let live_entries = cache.stats().live_entries as usize;
        if live_entries > CAPACITY {
            return Err(format!(
                "live_entries exceeded configured capacity: {live_entries} > {CAPACITY}"
            ));
        }
        max_live_entries = max_live_entries.max(live_entries);
    }

    let expected_retained_cold = CAPACITY - HOT_KEYS;
    let expected_evictions = COLD_INSERTS.saturating_sub(expected_retained_cold);

    assert_eq!(
        max_live_entries, CAPACITY,
        "cache should saturate at the configured bound under load"
    );
    assert_eq!(
        cache.stats().live_entries as usize,
        CAPACITY,
        "bounded cache should not accumulate more live entries than capacity"
    );
    assert_eq!(
        cache.stats().evictions_capacity as usize,
        expected_evictions,
        "steady-state insert churn should evict exactly the cold overflow set"
    );

    for hot_key in &hot_keys {
        assert!(
            matches!(cache.get(hot_key), MemoLookup::Hit { .. }),
            "hot working-set entry {hot_key:?} should survive sustained churn"
        );
    }

    for round in 0..expected_evictions {
        let cold_key = key(&format!("cold-{round}"));
        assert!(
            matches!(cache.get(&cold_key), MemoLookup::Miss),
            "old cold entry cold-{round} should have been evicted"
        );
    }

    for round in expected_evictions..COLD_INSERTS {
        let cold_key = key(&format!("cold-{round}"));
        assert!(
            matches!(cache.get(&cold_key), MemoLookup::Hit { .. }),
            "recent cold entry cold-{round} should still be resident"
        );
    }

    Ok(())
}
