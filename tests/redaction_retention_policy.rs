//! Exercise the exact cache, redactor, and resident policy together. Keeping
//! these source modules in this integration target gives the regression its
//! own production retention pool without mutating another test's process.

#[path = "../src/indexer/memoization.rs"]
pub(crate) mod memoization;
// This target uses part of the redactor; the library target lints the rest.
#[allow(dead_code)]
#[path = "../src/indexer/redact_secrets.rs"]
mod redact_secrets;
#[path = "../src/indexer/responsiveness/resident/policy.rs"]
mod resident_policy;

// The production redactor names its cache through crate::indexer. Re-export
// the actual module, not a replacement implementation or a fake allocator.
mod indexer {
    pub(crate) use super::memoization;
}

use redact_secrets::{MemoizingRedactor, update_redaction_retention_limit};
use resident_policy::{Decision, Policy, Sample};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

struct RestoreRetention;

impl Drop for RestoreRetention {
    fn drop(&mut self) {
        update_redaction_retention_limit(usize::MAX);
    }
}

fn apply(decision: Decision) {
    // This is the same handoff the shipping sampler makes, with no CPU-load
    // input and no substitute decision based on the test's expectations.
    update_redaction_retention_limit(
        usize::try_from(decision.inflight_byte_limit).unwrap_or(usize::MAX),
    );
}

fn payload(worker: usize, message: usize) -> String {
    format!(
        "worker {worker} message {message} {} password=hunter2hunter2",
        "padding λ ".repeat(3000),
    )
}

#[test]
#[serial_test::serial]
fn resident_policy_reclaims_live_redaction_values_and_holds_unknown_recovery() {
    update_redaction_retention_limit(usize::MAX);
    let _restore = RestoreRetention;
    let mut policy = Policy::default();
    let healthy = Sample {
        resident_bytes: Some(4 * GIB),
        available_bytes: Some(10 * GIB),
        total_bytes: Some(16 * GIB),
    };
    apply(policy.observe(healthy, None, 512 * MIB, MIB));

    let mut first = MemoizingRedactor::new();
    let mut second = MemoizingRedactor::new();
    for message in 0..64 {
        for (worker, redactor) in [(0, &mut first), (1, &mut second)] {
            let input = payload(worker, message);
            assert!(input.len() < MemoizingRedactor::MAX_MEMOIZED_INPUT_BYTES);
            assert_eq!(
                redactor.redact_text(&input),
                redact_secrets::redact_text(&input)
            );
        }
    }
    assert_eq!(first.stats().live_entries, 64);
    assert_eq!(second.stats().live_entries, 64);

    let severe = policy.observe(
        Sample {
            resident_bytes: Some(16 * GIB),
            ..healthy
        },
        None,
        512 * MIB,
        MIB,
    );
    assert_eq!(severe.capacity_pct, 1);
    assert_eq!(severe.inflight_byte_limit, MIB);
    apply(severe);

    // Each owner retained more than one MiB. The first cannot make the
    // aggregate fit while the second is idle, so it sheds all its own values.
    // The second then evicts only the oldest prefix needed to fit the pool.
    // Neither clean nor empty input should create a cache lookup/insert audit.
    let (clean, audit) = first.redact_text_with_audit("ordinary clean text");
    assert_eq!(clean, "ordinary clean text");
    assert!(audit.is_empty());
    assert_eq!(first.stats().live_entries, 0);
    let (empty, audit) = second.redact_text_with_audit("");
    assert!(empty.is_empty());
    assert!(audit.is_empty());
    assert!(second.stats().live_entries > 0);
    assert!(second.stats().live_entries < 64);

    let input = payload(0, 0);
    let misses = first.stats().misses;
    assert_eq!(
        first.redact_text(&input),
        redact_secrets::redact_text(&input)
    );
    assert_eq!(first.stats().misses, misses + 1);
    assert_eq!(first.stats().hits, 0, "an evicted value must be recomputed");

    let unknown = policy.observe(Sample::default(), None, 512 * MIB, MIB);
    assert_eq!(unknown.reason, "resident_memory_unknown");
    assert_eq!(unknown.inflight_byte_limit, MIB);
    apply(unknown);
    for _ in 0..2 {
        let held = policy.observe(healthy, None, 512 * MIB, MIB);
        assert_eq!(held.inflight_byte_limit, MIB);
        apply(held);
    }
    let recovered = policy.observe(healthy, None, 512 * MIB, MIB);
    assert_eq!(recovered.reason, "resident_memory_recovery");
    assert!(recovered.inflight_byte_limit > MIB);
    apply(recovered);

    let input = payload(0, 1000);
    let expected = redact_secrets::redact_text(&input).into_owned();
    let hits = first.stats().hits;
    assert_eq!(first.redact_text(&input), expected);
    assert_eq!(first.stats().hits, hits);
    assert_eq!(first.redact_text(&input), expected);
    assert_eq!(first.stats().hits, hits + 1);
}
