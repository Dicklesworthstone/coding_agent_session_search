use coding_agent_search::indexer::{IndexOptions, run_index};
use coding_agent_search::search::tantivy::index_dir;
use criterion::{Criterion, criterion_group, criterion_main};
use std::fs;
use std::io::Write;
use tempfile::TempDir;

fn bench_index_full(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let db_path = data_dir.join("agent_search.db");
    let sample_dir = data_dir.join("sample_logs");
    fs::create_dir_all(&sample_dir).unwrap();
    let mut f = fs::File::create(sample_dir.join("rollout-1.jsonl")).unwrap();
    writeln!(f, "{{\"role\":\"user\",\"content\":\"hello\"}}").unwrap();
    writeln!(f, "{{\"role\":\"assistant\",\"content\":\"world\"}}").unwrap();

    let opts = IndexOptions {
        full: true,
        force_rebuild: true,
        watch: false,
        watch_once_paths: None,
        db_path,
        data_dir: data_dir.clone(),
        progress: None,
    };

    // create empty index dir so Tantivy opens cleanly
    let _ = index_dir(&data_dir);

    c.bench_function("index_full_empty", |b| {
        b.iter(|| run_index(opts.clone(), None))
    });
}

criterion_group!(benches, bench_index_full);
criterion_main!(benches);
