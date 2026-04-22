//! Post-flip A/B measurement of Cards 1/2/3 defaults on a realistic ingest.
//!
//! Direct-test variant of `bench_card_defaults_ab` in
//! `benches/index_perf.rs`. Criterion's framework + the sibling build.rs
//! path kept leaving orphaned bench binaries pinned at 97% CPU on a
//! shared 128-core box; we measure wall-clock once per cell here
//! instead. One timed iteration per cell, small corpus, rayon pool
//! capped upstream via `RAYON_NUM_THREADS`.
//!
//! Run with:
//!
//! ```text
//! CARGO_TARGET_DIR=target-perf RAYON_NUM_THREADS=4 \
//!   cargo test --release --test card_defaults_wallclock_ab \
//!   -- --ignored --nocapture
//! ```

use coding_agent_search::indexer::{IndexOptions, run_index};
use std::fs;
use std::time::Instant;
use tempfile::TempDir;

fn create_corpus(tmp: &TempDir, count: usize) -> (std::path::PathBuf, std::path::PathBuf) {
    let data_dir = tmp.path().join("data");
    let db_path = data_dir.join("agent_search.db");

    // Each message body is ~2 KiB of realistic mixed text so real
    // indexing work dominates the fixed setup overhead. Unique tokens
    // per message keep the Tantivy postings list growing, matching how
    // real corpora stress the writer.
    let filler = (0..256)
        .map(|i| format!("token_{i}_abc{i}xyz"))
        .collect::<Vec<_>>()
        .join(" ");

    for i in 0..count {
        let date_path = format!("sessions/2024/11/{:02}", (i % 30) + 1);
        let sessions = data_dir.join(&date_path);
        fs::create_dir_all(&sessions).unwrap();

        let filename = format!("rollout-{i}.jsonl");
        let file = sessions.join(&filename);
        let ts = 1732118400000_u64 + (i as u64 * 1000);
        let user_body = format!("user_msg_{i}_unique_{i} {filler} alpha_{i}");
        let asst_body = format!("assistant_reply_{i}_unique_{i} {filler} omega_{i}");
        let content = format!(
            "{{\"type\": \"event_msg\", \"timestamp\": {ts}, \"payload\": {{\"type\": \"user_message\", \"message\": {user_body:?}}}}}\n{{\"type\": \"response_item\", \"timestamp\": {}, \"payload\": {{\"role\": \"assistant\", \"content\": {asst_body:?}}}}}\n",
            ts + 1000
        );
        fs::write(file, content).unwrap();
    }

    (data_dir, db_path)
}

struct EnvGuard {
    keys: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn set(pairs: &[(&'static str, &str)]) -> Self {
        let mut keys = Vec::with_capacity(pairs.len());
        for &(k, v) in pairs {
            let previous = std::env::var(k).ok();
            // SAFETY: #[ignore] test, single-threaded.
            unsafe {
                std::env::set_var(k, v);
            }
            keys.push((k, previous));
        }
        Self { keys }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            for (k, prev) in &self.keys {
                match prev {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

fn run_cell(label: &str, governor: &str, combine: &str, shadow: &str, corpus_size: usize) -> u128 {
    let tmp = TempDir::new().unwrap();
    let (data_dir, db_path) = create_corpus(&tmp, corpus_size);
    let fake_home = tmp.path().join("home");
    let fake_xdg = tmp.path().join("xdg");
    fs::create_dir_all(&fake_home).unwrap();
    fs::create_dir_all(&fake_xdg).unwrap();

    // Full rebuild path matches the original `bench_card_defaults_ab`
    // intent. The trailing FTS repair used to fail with "callback
    // requested query abort" under frankensqlite on fresh test DBs;
    // that's fixed upstream (indexer opens a fresh storage for the
    // repair) so run_index returns Ok here. We still use
    // `force_rebuild=true` + `full=true` to exercise the full ingest +
    // Tantivy commit + FTS repair path end to end.
    let opts = IndexOptions {
        full: true,
        force_rebuild: true,
        watch: false,
        watch_once_paths: None,
        db_path,
        data_dir: data_dir.clone(),
        semantic: false,
        build_hnsw: false,
        embedder: "fastembed".to_string(),
        progress: None,
        watch_interval_secs: 30,
    };

    // Critical: without CASS_IGNORE_SOURCES_CONFIG=1 + a private HOME,
    // `run_index` loads the global sources config and scans EVERY
    // agent dir the user has (~/.codex, ~/.claude, ~/.cursor, ...),
    // not just this test's temp corpus. On a box with 500k+ sessions
    // this is what made earlier runs appear to wedge for 30+ minutes.
    let _guard = EnvGuard::set(&[
        ("CASS_RESPONSIVENESS_CALIBRATION", governor),
        ("CASS_STREAMING_CONSUMER_COMBINE", combine),
        ("CASS_INDEXER_PARALLEL_WAL", shadow),
        ("CASS_IGNORE_SOURCES_CONFIG", "1"),
        ("HOME", fake_home.to_str().unwrap()),
        ("XDG_DATA_HOME", fake_xdg.to_str().unwrap()),
    ]);

    let t0 = Instant::now();
    let r = run_index(opts, None);
    let elapsed_us = t0.elapsed().as_micros();
    let (ok, err_msg) = match &r {
        Ok(_) => (true, String::new()),
        Err(e) => (false, format!("{e:#}")),
    };
    eprintln!(
        "RESULT cell={label} governor={governor} combine={combine} shadow={shadow} \
         elapsed_us={elapsed_us} elapsed_ms={} ok={ok} err={err_msg}",
        elapsed_us / 1000,
    );
    // flush to the .output file promptly so a wedge is visible.
    use std::io::Write;
    let _ = std::io::stderr().flush();
    elapsed_us
}

#[test]
#[ignore = "wall-clock A/B; run with --ignored --nocapture in release mode"]
fn card_defaults_wallclock_ab_4cell() {
    eprintln!("=== card_defaults_wallclock_ab_4cell starting ===");
    use std::io::Write;
    let _ = std::io::stderr().flush();

    // Two corpus sizes so we can isolate the real indexing cost from
    // the fixed per-run setup cost (storage open, governor spin-up,
    // Tantivy writer open, etc.). Per-corpus throughput cancels the
    // fixed overhead:
    //     indexing_per_conv = (T_big - T_small) / (big - small)
    let small_corpus = 100;
    let big_corpus = 10_000;

    let cells: [(&str, &str, &str, &str); 4] = [
        ("legacy_all_off", "static", "0", "off"),
        ("new_all_on", "conformal", "1", "shadow"),
        ("only_combine_on", "static", "1", "off"),
        ("only_governor_on", "conformal", "0", "off"),
    ];

    struct Row {
        label: String,
        small_us: u128,
        big_us: u128,
        per_conv_us: f64,
    }

    let reps: usize = 5;
    fn median_u128(mut xs: Vec<u128>) -> u128 {
        assert!(!xs.is_empty(), "median of empty slice is undefined");
        xs.sort();
        let n = xs.len();
        if n.is_multiple_of(2) {
            // Average the two middle values (integer average rounded down).
            (xs[n / 2 - 1] + xs[n / 2]) / 2
        } else {
            xs[n / 2]
        }
    }

    let mut rows: Vec<Row> = Vec::with_capacity(cells.len());
    for &(label, governor, combine, shadow) in &cells {
        // Warm once so first-touch allocator/file-cache cost doesn't
        // land entirely on whichever cell runs first in the array.
        let _ = run_cell(
            &format!("{label}.warmup"),
            governor,
            combine,
            shadow,
            small_corpus,
        );
        let mut small_samples: Vec<u128> = Vec::with_capacity(reps);
        let mut big_samples: Vec<u128> = Vec::with_capacity(reps);
        for i in 0..reps {
            small_samples.push(run_cell(
                &format!("{label}.small.{i}"),
                governor,
                combine,
                shadow,
                small_corpus,
            ));
            big_samples.push(run_cell(
                &format!("{label}.big.{i}"),
                governor,
                combine,
                shadow,
                big_corpus,
            ));
        }
        let small_us = median_u128(small_samples);
        let big_us = median_u128(big_samples);
        let delta_us = big_us.saturating_sub(small_us) as f64;
        let delta_convs = (big_corpus - small_corpus) as f64;
        let per_conv_us = if delta_convs > 0.0 {
            delta_us / delta_convs
        } else {
            0.0
        };
        rows.push(Row {
            label: label.to_string(),
            small_us,
            big_us,
            per_conv_us,
        });
    }

    eprintln!(
        "\n==== card_defaults_wallclock_ab summary (small={small_corpus}, big={big_corpus}) ===="
    );
    let baseline = rows
        .iter()
        .find(|r| r.label == "legacy_all_off")
        .map(|r| r.per_conv_us)
        .unwrap_or(1.0)
        .max(1.0);
    eprintln!(
        "{:<20} {:>10} {:>12} {:>12} {:>10}",
        "cell", "small_us", "big_us", "per_conv_us", "vs_legacy"
    );
    for r in &rows {
        let pct = 100.0 * r.per_conv_us / baseline;
        eprintln!(
            "{:<20} {:>10} {:>12} {:>12.1} {:>9.1}%",
            r.label, r.small_us, r.big_us, r.per_conv_us, pct
        );
    }
    let _ = std::io::stderr().flush();
}
