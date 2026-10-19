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

const WALLCLOCK_CELL_CHILD_ENV: &str = "CASS_TEST_WALLCLOCK_CELL_CHILD";
const WALLCLOCK_CELL_LABEL_ENV: &str = "CASS_TEST_WALLCLOCK_CELL_LABEL";
const WALLCLOCK_CELL_CORPUS_ENV: &str = "CASS_TEST_WALLCLOCK_CELL_CORPUS";
const WALLCLOCK_CELL_ELAPSED_PREFIX: &str = "WALLCLOCK_CELL_ELAPSED_US=";

fn run_cell(label: &str, governor: &str, combine: &str, shadow: &str, corpus_size: usize) -> u128 {
    let tmp = TempDir::new().unwrap();
    let fixture_home = tmp.path().join("home");
    let fixture_xdg = tmp.path().join("xdg");
    fs::create_dir_all(&fixture_home).unwrap();
    fs::create_dir_all(&fixture_xdg).unwrap();

    let mut cmd = std::process::Command::new(
        std::env::current_exe().expect("current card_defaults_wallclock_ab binary"),
    );
    cmd.arg("--exact")
        .arg("card_defaults_wallclock_ab_cell_child")
        .arg("--nocapture")
        .current_dir(tmp.path())
        .env_clear()
        .env(WALLCLOCK_CELL_CHILD_ENV, "1")
        .env(WALLCLOCK_CELL_LABEL_ENV, label)
        .env(WALLCLOCK_CELL_CORPUS_ENV, corpus_size.to_string())
        .env("CASS_RESPONSIVENESS_CALIBRATION", governor)
        .env("CASS_STREAMING_CONSUMER_COMBINE", combine)
        .env("CASS_INDEXER_PARALLEL_WAL", shadow)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("HOME", &fixture_home)
        .env("XDG_DATA_HOME", &fixture_xdg)
        .env("RUST_MIN_STACK", "16777216");
    if let Some(rayon_threads) = std::env::var_os("RAYON_NUM_THREADS") {
        cmd.env("RAYON_NUM_THREADS", rayon_threads);
    }
    let output = cmd.output().expect("spawn isolated wallclock cell child");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "isolated wallclock cell child failed for {label}\nstatus={:?}\n{combined}",
        output.status
    );
    eprint!("{combined}");
    use std::io::Write;
    let _ = std::io::stderr().flush();
    combined
        .lines()
        .find_map(|line| line.strip_prefix(WALLCLOCK_CELL_ELAPSED_PREFIX))
        .and_then(|raw| raw.trim().parse::<u128>().ok())
        .unwrap_or_else(|| {
            panic!("missing {WALLCLOCK_CELL_ELAPSED_PREFIX} in child output: {combined}")
        })
}

#[test]
fn card_defaults_wallclock_ab_cell_child() {
    if std::env::var_os(WALLCLOCK_CELL_CHILD_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }
    let label = std::env::var(WALLCLOCK_CELL_LABEL_ENV).expect("cell label");
    let corpus_size: usize = std::env::var(WALLCLOCK_CELL_CORPUS_ENV)
        .expect("cell corpus size")
        .parse()
        .expect("parse corpus size");
    let governor = std::env::var("CASS_RESPONSIVENESS_CALIBRATION").unwrap_or_default();
    let combine = std::env::var("CASS_STREAMING_CONSUMER_COMBINE").unwrap_or_default();
    let shadow = std::env::var("CASS_INDEXER_PARALLEL_WAL").unwrap_or_default();

    let tmp = TempDir::new().unwrap();
    let (data_dir, db_path) = create_corpus(&tmp, corpus_size);
    let opts = IndexOptions {
        full: true,
        force_rebuild: true,
        watch: false,
        watch_once_paths: None,
        db_path,
        data_dir,
        semantic: false,
        build_hnsw: false,
        embedder: "fastembed".to_string(),
        progress: None,
        watch_interval_secs: 30,
    };

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
    eprintln!("{WALLCLOCK_CELL_ELAPSED_PREFIX}{elapsed_us}");
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
