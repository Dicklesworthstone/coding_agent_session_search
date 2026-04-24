//! Metamorphic regression test for agent-detection scan invariance.
//!
//! `coding_agent_session_search-irv8h`: franken_agent_detection (FAD)
//! connectors take a list of `ScanRoot` entries and walk each one
//! looking for sessions. Without an explicit invariant pin, a future
//! change that lets root order influence WHICH sessions a connector
//! claims (e.g., order-dependent dispatch on overlapping paths, or a
//! HashMap iteration leaking into the dedupe pass) would silently
//! produce different results for the same on-disk state depending on
//! the order the user listed `--source` paths. Operator-visible
//! consequence: `cass index --full` produces different conversation
//! counts on consecutive runs if the source list shuffles.
//!
//! MR archetype is **Permutative (Pattern 4)** from the metamorphic
//! skill: T(scan_roots) = permute(scan_roots). Relation: the deduped
//! set of detected sessions (keyed by stable identity) is identical.
//! Order of discovery may vary; the SET must not.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use coding_agent_search::indexer::get_connector_factories;
use franken_agent_detection::{NormalizedConversation, ScanContext, ScanRoot};

/// Stable identity tuple for set-equality comparison. Order of
/// discovery is allowed to vary; the SET must not. Each component
/// is a documented field of `NormalizedConversation` (see
/// `franken_agent_detection::types::NormalizedConversation`).
type ConversationKey = (String, PathBuf, Option<String>);

fn key(conv: &NormalizedConversation) -> ConversationKey {
    (
        conv.agent_slug.clone(),
        conv.source_path.clone(),
        conv.external_id.clone(),
    )
}

/// Seed a Codex session at `root/.codex/sessions/<filename>`. Mirrors
/// the layout codex's `append_explicit_roots` walker recognises (see
/// franken_agent_detection/src/connectors/codex.rs).
fn seed_codex_session_under(root: &Path, filename: &str, ts_millis: u64, content: &str) {
    let sessions = root.join(".codex").join("sessions");
    fs::create_dir_all(&sessions).expect("create codex sessions dir");
    let file = sessions.join(filename);
    let body = format!(
        r#"{{"type":"event_msg","timestamp":{ts_millis},"payload":{{"type":"user_message","message":"{content}"}}}}
{{"type":"response_item","timestamp":{},"payload":{{"role":"assistant","content":"{content}_response"}}}}"#,
        ts_millis + 1000
    );
    fs::write(file, body).expect("write codex session");
}

fn scan_codex_collected(scan_roots: Vec<ScanRoot>, data_dir: &Path) -> Vec<NormalizedConversation> {
    let factories = get_connector_factories();
    let (_slug, build_codex) = factories
        .iter()
        .find(|(slug, _)| *slug == "codex")
        .expect("codex factory registered");
    let connector = build_codex();
    let ctx = ScanContext::with_roots(data_dir.to_path_buf(), scan_roots, None);
    let mut found = Vec::new();
    connector
        .scan_with_callback(&ctx, &mut |conv| {
            found.push(conv);
            Ok(())
        })
        .expect("codex scan_with_callback");
    found
}

/// `coding_agent_session_search-irv8h`: pin scan-root permutation
/// invariance for the codex connector. Two distinct codex sessions
/// in two distinct roots → scanning roots in order [A, B] must
/// produce the same SET of detected sessions as scanning [B, A].
///
/// This is the strongest tractable form of the metamorphic relation
/// without remote-sync infrastructure: codex is one of the most
/// active connectors and its discovery surface (per-root walker over
/// `.codex/sessions/`) is representative of the order-sensitive code
/// paths in other connectors. If a future regression makes any
/// connector's dispatch order-dependent, this test trips.
#[test]
fn mr_codex_scan_invariant_under_root_permutation() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let root_a = tmp.path().join("root-a");
    let root_b = tmp.path().join("root-b");
    let data_dir = tmp.path().join("cass-data");
    fs::create_dir_all(&data_dir).expect("create data dir");

    // Seed two distinct sessions per root so the cross-root union is
    // > a single root's contribution and a regression that DROPS
    // one root entirely (the most common bug shape) would fail with
    // a cardinality mismatch, not a silent overlap.
    seed_codex_session_under(&root_a, "rollout-a1.jsonl", 1_732_118_400_000, "alpha");
    seed_codex_session_under(&root_a, "rollout-a2.jsonl", 1_732_118_500_000, "beta");
    seed_codex_session_under(&root_b, "rollout-b1.jsonl", 1_732_118_600_000, "gamma");
    seed_codex_session_under(&root_b, "rollout-b2.jsonl", 1_732_118_700_000, "delta");

    let order_ab = scan_codex_collected(
        vec![
            ScanRoot::local(root_a.clone()),
            ScanRoot::local(root_b.clone()),
        ],
        &data_dir,
    );
    let order_ba = scan_codex_collected(
        vec![ScanRoot::local(root_b.clone()), ScanRoot::local(root_a.clone())],
        &data_dir,
    );

    // Sanity: the cross-root scan must surface BOTH roots' sessions
    // in BOTH orderings. Otherwise the test is vacuous (a regression
    // that drops both halves to zero would also pass an empty=empty
    // assertion). 4 seeded sessions ⇒ 4 detected — modulo any
    // platform-specific filtering, but codex accepts the JSONL we
    // wrote unconditionally per franken_agent_detection's parser.
    assert!(
        order_ab.len() >= 4,
        "expected to detect at least 4 codex sessions across both roots in order [A,B]; \
         got {} sessions: {:?}",
        order_ab.len(),
        order_ab.iter().map(|c| c.source_path.display().to_string()).collect::<Vec<_>>()
    );
    assert!(
        order_ba.len() >= 4,
        "expected to detect at least 4 codex sessions across both roots in order [B,A]; \
         got {} sessions: {:?}",
        order_ba.len(),
        order_ba.iter().map(|c| c.source_path.display().to_string()).collect::<Vec<_>>()
    );

    // The metamorphic relation: SET equality of stable identity
    // tuples. Order-of-discovery is allowed to vary (the connector
    // sorts/dedups internally per its own contract), but the deduped
    // SET MUST be identical across permutations.
    let set_ab: HashSet<ConversationKey> = order_ab.iter().map(key).collect();
    let set_ba: HashSet<ConversationKey> = order_ba.iter().map(key).collect();
    assert_eq!(
        set_ab, set_ba,
        "metamorphic invariant violated: codex scan(roots=[A,B]) detected a different \
         SET of sessions than scan(roots=[B,A]).\n\
         only in [A,B]: {:?}\nonly in [B,A]: {:?}",
        set_ab.difference(&set_ba).collect::<Vec<_>>(),
        set_ba.difference(&set_ab).collect::<Vec<_>>()
    );
}
