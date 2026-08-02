//! Redaction hot-path benchmark (harness = false, hand-rolled timing).
//!
//! Measures the exact production persist-time transform
//! (`persist::map_batch_to_internal`: rayon pool + per-worker
//! `MemoizingRedactor`) over real session fixtures, plus two reference
//! modes that bracket it:
//!
//! - `memoized`  — production path (rayon + MemoizingRedactor)
//! - `uncached`  — same transform with the plain `redact_text` path
//!   (no memo cache), parallel across conversations
//! - `off`       — redaction disabled (`CASS_INDEX_REDACTION=off`):
//!   pure clone/remap floor
//!
//! Configuration is via env vars (robust against cargo-injected CLI
//! args like `--bench`):
//!
//! - `CASS_REDACT_BENCH_DIR`    directory of *.jsonl codex rollout
//!   fixtures (scanned with the real codex connector). Required.
//! - `CASS_REDACT_BENCH_ITERS`  timed iterations per mode (default 5)
//! - `CASS_REDACT_BENCH_WARMUP` warmup iterations per mode (default 1)
//! - `CASS_REDACT_BENCH_MODES`  comma list from {memoized,uncached,off}
//!   (default all three)
//!
//! Output is one parseable line per iteration plus a `median` summary
//! line per mode:
//!   `redaction_perf mode=memoized iter=1 secs=1.234`
//!   `redaction_perf mode=memoized median_secs=1.234 msgs=... content_mb=... mb_per_s=...`
//!
//! Run (profile knobs pinned so runs are comparable without a fat-LTO
//! rebuild):
//! ```bash
//! CARGO_PROFILE_BENCH_LTO=off CARGO_PROFILE_BENCH_CODEGEN_UNITS=8 \
//! CARGO_PROFILE_BENCH_DEBUG=line-tables-only CARGO_PROFILE_BENCH_STRIP=false \
//! CASS_REDACT_BENCH_DIR=/path/to/fixtures \
//! cargo bench --bench redaction_perf
//! ```

use std::path::PathBuf;
use std::time::Instant;

use coding_agent_search::connectors::{NormalizedConversation, ScanContext, ScanRoot};
use coding_agent_search::indexer::get_connector_factories;
use coding_agent_search::indexer::persist::{bench_map_batch_to_internal, map_to_internal};
use rayon::prelude::*;

fn env_usize(key: &str, default: usize) -> usize {
    dotenvy::var(key)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

fn load_fixture_conversations(dir: &PathBuf) -> Vec<NormalizedConversation> {
    let factories = get_connector_factories();
    let (_slug, build_codex) = factories
        .iter()
        .find(|(slug, _)| *slug == "codex")
        .expect("codex factory registered");
    let connector = build_codex();
    let ctx = ScanContext::with_roots(dir.clone(), vec![ScanRoot::local(dir.clone())], None);
    let mut convs = Vec::new();
    connector
        .scan_with_callback(&ctx, &mut |conversation| {
            convs.push(conversation);
            Ok(())
        })
        .expect("codex fixture scan");
    convs
}

fn corpus_stats(convs: &[NormalizedConversation]) -> (usize, usize) {
    let msgs: usize = convs.iter().map(|c| c.messages.len()).sum();
    let bytes: usize = convs
        .iter()
        .map(|c| {
            c.messages
                .iter()
                .map(|m| m.content.len())
                .sum::<usize>()
        })
        .sum();
    (msgs, bytes)
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).expect("finite timing"));
    let n = xs.len();
    if n == 0 {
        return f64::NAN;
    }
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    }
}

/// Order-stable digest of every redacted output field. This is the
/// cross-round behavior proof on REAL data: for a fixed fixture dir the
/// digest must be identical before and after every optimization round
/// (mode `memoized` and `uncached` must also agree with each other).
fn output_digest(convs: &[coding_agent_search::model::types::Conversation]) -> String {
    let mut hasher = blake3::Hasher::new();
    for conv in convs {
        hasher.update(conv.title.as_deref().unwrap_or("\u{0}").as_bytes());
        hasher.update(&[0]);
        hasher.update(
            serde_json::to_string(&conv.metadata_json)
                .unwrap_or_default()
                .as_bytes(),
        );
        hasher.update(&[0]);
        for msg in &conv.messages {
            hasher.update(msg.content.as_bytes());
            hasher.update(&[0]);
            hasher.update(
                serde_json::to_string(&msg.extra_json)
                    .unwrap_or_default()
                    .as_bytes(),
            );
            hasher.update(&[0]);
            for snippet in &msg.snippets {
                hasher.update(snippet.snippet_text.as_deref().unwrap_or("\u{0}").as_bytes());
                hasher.update(&[0]);
            }
        }
    }
    hasher.finalize().to_hex().to_string()
}

fn run_mode(
    mode: &str,
    convs: &[NormalizedConversation],
    iters: usize,
    warmup: usize,
    msgs: usize,
    bytes: usize,
) {
    type MappedBatch = Vec<coding_agent_search::model::types::Conversation>;
    let work: Box<dyn Fn() -> MappedBatch> = match mode {
        // Exact production path: rayon pool + per-worker MemoizingRedactor.
        "memoized" => Box::new(|| bench_map_batch_to_internal(convs)),
        // Plain (non-memoized) redaction path, parallel across conversations.
        "uncached" => Box::new(|| convs.par_iter().map(map_to_internal).collect()),
        // Redaction disabled: clone/remap floor (main() forces
        // CASS_INDEX_REDACTION per mode before calling run_mode).
        "off" => Box::new(|| bench_map_batch_to_internal(convs)),
        other => panic!("unknown bench mode: {other}"),
    };

    for _ in 0..warmup {
        let out = work();
        let n: usize = out.iter().map(|c| c.messages.len()).sum();
        assert_eq!(n, msgs, "mapped message count must match corpus");
    }
    let mut samples = Vec::with_capacity(iters);
    let mut digest = String::new();
    for iter in 0..iters {
        let start = Instant::now();
        let out = work();
        let secs = start.elapsed().as_secs_f64();
        let n: usize = out.iter().map(|c| c.messages.len()).sum();
        assert_eq!(n, msgs, "mapped message count must match corpus");
        samples.push(secs);
        println!("redaction_perf mode={mode} iter={iter} secs={secs:.4}");
        if iter == 0 {
            digest = output_digest(&out);
        }
    }
    let med = median(samples);
    let mb = bytes as f64 / (1024.0 * 1024.0);
    println!(
        "redaction_perf mode={mode} median_secs={med:.4} msgs={msgs} content_mb={mb:.1} mb_per_s={:.1} output_digest={digest}",
        mb / med
    );
}

fn main() {
    let dir = PathBuf::from(
        dotenvy::var("CASS_REDACT_BENCH_DIR").expect("CASS_REDACT_BENCH_DIR must point at fixture dir"),
    );
    let iters = env_usize("CASS_REDACT_BENCH_ITERS", 5);
    let warmup = env_usize("CASS_REDACT_BENCH_WARMUP", 1);
    let modes_raw = dotenvy::var("CASS_REDACT_BENCH_MODES")
        .unwrap_or_else(|_| "memoized,uncached,off".to_string());

    let t_load = Instant::now();
    let convs = load_fixture_conversations(&dir);
    let load_secs = t_load.elapsed().as_secs_f64();
    let (msgs, bytes) = corpus_stats(&convs);
    println!(
        "redaction_perf corpus dir={} convs={} msgs={} content_mb={:.1} load_secs={:.2} rayon_threads={}",
        dir.display(),
        convs.len(),
        msgs,
        bytes as f64 / (1024.0 * 1024.0),
        load_secs,
        rayon::current_num_threads(),
    );
    assert!(!convs.is_empty(), "fixture dir produced no conversations");

    for mode in modes_raw.split(',').map(str::trim).filter(|m| !m.is_empty()) {
        // Drive redaction through the PRIMARY switch and clear the legacy
        // one: CASS_INDEX_REDACTION outranks CASS_REDACT_SECRETS, so an
        // ambient value of either could otherwise silently corrupt a mode
        // (ambient `full` breaks the off-floor; ambient `off` breaks the
        // memoized/uncached headline numbers). Setting both per mode makes
        // the bench deterministic regardless of the caller's environment.
        // SAFETY: single-threaded configuration point before the mode's
        // rayon work begins; matches index_perf.rs precedent.
        unsafe {
            std::env::set_var(
                "CASS_INDEX_REDACTION",
                if mode == "off" { "off" } else { "full" },
            );
            std::env::remove_var("CASS_REDACT_SECRETS");
        }
        run_mode(mode, &convs, iters, warmup, msgs, bytes);
    }
}
