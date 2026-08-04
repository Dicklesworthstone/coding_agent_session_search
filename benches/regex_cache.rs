use coding_agent_search::search::tantivy::{build_schema, fields_from_schema};
use criterion::{Criterion, criterion_group, criterion_main};
use frankensearch::lexical_tantivy::{Field, cass_regex_query_cached, cass_regex_query_uncached};
use std::hint::black_box;

fn content_field() -> Field {
    let schema = build_schema();
    fields_from_schema(&schema).expect("fields").content
}

fn bench_regex_cache_hits(c: &mut Criterion) {
    let field = content_field();
    let patterns = [
        ("regex_cache_hit_prefix", "test.*"),
        ("regex_cache_hit_suffix", ".*\\.rs"),
        ("regex_cache_hit_substring", ".*error.*"),
        ("regex_cache_hit_complex", ".*foo.*bar.*"),
    ];

    for (name, pattern) in patterns {
        let _ = cass_regex_query_cached(field, pattern).expect("warm cache");
        c.bench_function(name, |b| {
            b.iter(|| {
                let query = cass_regex_query_cached(field, pattern).expect("cached");
                black_box(query);
            });
        });
    }
}

fn bench_regex_cache_misses(c: &mut Criterion) {
    let field = content_field();

    c.bench_function("regex_cache_miss_prefix", |b| {
        let mut counter = 0u64;
        b.iter(|| {
            counter += 1;
            let pattern = format!("test{}.*", counter);
            let query = cass_regex_query_cached(field, &pattern).expect("cache miss");
            black_box(query);
        });
    });

    c.bench_function("regex_cache_miss_suffix", |b| {
        let mut counter = 0u64;
        b.iter(|| {
            counter += 1;
            let pattern = format!(".*file{}\\.rs", counter);
            let query = cass_regex_query_cached(field, &pattern).expect("cache miss");
            black_box(query);
        });
    });

    c.bench_function("regex_cache_miss_substring", |b| {
        let mut counter = 0u64;
        b.iter(|| {
            counter += 1;
            let pattern = format!(".*error{}.*", counter);
            let query = cass_regex_query_cached(field, &pattern).expect("cache miss");
            black_box(query);
        });
    });

    c.bench_function("regex_cache_miss_complex", |b| {
        let mut counter = 0u64;
        b.iter(|| {
            counter += 1;
            let pattern = format!(".*foo{}.*bar{}.*", counter, counter + 1);
            let query = cass_regex_query_cached(field, &pattern).expect("cache miss");
            black_box(query);
        });
    });
}

fn bench_regex_uncached(c: &mut Criterion) {
    let field = content_field();
    let patterns = [
        ("regex_uncached_prefix", "test.*"),
        ("regex_uncached_suffix", ".*\\.rs"),
        ("regex_uncached_substring", ".*error.*"),
        ("regex_uncached_complex", ".*foo.*bar.*"),
    ];

    for (name, pattern) in patterns {
        c.bench_function(name, |b| {
            b.iter(|| {
                let query = cass_regex_query_uncached(field, pattern).expect("uncached");
                black_box(query);
            });
        });
    }
}

fn bench_regex_typing_sequence(c: &mut Criterion) {
    let field = content_field();
    let sequence = [".*err.*", ".*erro.*", ".*error.*", ".*erro.*", ".*err.*"];

    // Pre-warm the first pattern so the sequence mixes hits and misses like real typing.
    let _ = cass_regex_query_cached(field, sequence[0]).expect("warm");

    c.bench_function("regex_cache_typing_sequence", |b| {
        b.iter(|| {
            for pattern in &sequence {
                let query = cass_regex_query_cached(field, pattern).expect("sequence");
                black_box(query);
            }
        });
    });

    c.bench_function("regex_uncached_typing_sequence", |b| {
        b.iter(|| {
            for pattern in &sequence {
                let query = cass_regex_query_uncached(field, pattern).expect("sequence");
                black_box(query);
            }
        });
    });
}

criterion_group!(
    benches,
    bench_regex_cache_hits,
    bench_regex_cache_misses,
    bench_regex_uncached,
    bench_regex_typing_sequence
);
criterion_main!(benches);
