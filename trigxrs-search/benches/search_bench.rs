use std::io::{Cursor, Write};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tempfile::NamedTempFile;

use trigxrs_index::IndexBuilder;
use trigxrs_search::{IndexData, SearchOptions};

/// Generate deterministic content with searchable patterns.
fn generate_content(file_idx: usize, size: usize) -> Vec<u8> {
    let mut content = Vec::with_capacity(size);
    let mut i = 0u32;
    while content.len() < size {
        let line = format!(
            "file{}_line{}: func main() {{ val := compute(x_{}, y_{}); println(result) }}\n",
            file_idx, i, i, i + 1
        );
        content.extend_from_slice(line.as_bytes());
        i += 1;
    }
    content.truncate(size);
    content
}

/// Build an index of `file_count` files, each `file_size` bytes, return (IndexData, temp).
fn setup_index(file_count: usize, file_size: usize) -> (IndexData, NamedTempFile) {
    let mut builder = IndexBuilder::new();
    for i in 0..file_count {
        let content = generate_content(i, file_size);
        let path = format!("src/file_{}.rs", i);
        builder.add_file(&path, &content).unwrap();
    }

    let mut temp = NamedTempFile::new().unwrap();
    {
        let mut buf = Cursor::new(Vec::new());
        builder.write_shard(&mut buf).unwrap();
        temp.write_all(&buf.into_inner()).unwrap();
    }

    let index = IndexData::open(temp.path()).unwrap();
    (index, temp)
}

fn bench_index_build_small(c: &mut Criterion) {
    c.bench_function("index_build_10x1KB", |b| {
        b.iter(|| {
            let mut builder = IndexBuilder::new();
            for i in 0..10 {
                let content = generate_content(i, 1024);
                builder.add_file(&format!("f{}.rs", i), &content).unwrap();
            }
            let mut buf = Cursor::new(Vec::new());
            builder.write_shard(&mut buf).unwrap();
            black_box(buf.into_inner().len());
        });
    });
}

fn bench_index_build_medium(c: &mut Criterion) {
    c.bench_function("index_build_100x10KB", |b| {
        b.iter(|| {
            let mut builder = IndexBuilder::new();
            for i in 0..100 {
                let content = generate_content(i, 10 * 1024);
                builder.add_file(&format!("f{}.rs", i), &content).unwrap();
            }
            let mut buf = Cursor::new(Vec::new());
            builder.write_shard(&mut buf).unwrap();
            black_box(buf.into_inner().len());
        });
    });
}

fn bench_literal_search_short(c: &mut Criterion) {
    let (index, _tmp) = setup_index(50, 5 * 1024);
    c.bench_function("literal_search_3char", |b| {
        b.iter(|| {
            let result = index.search_literal(black_box("val"), true, 0).unwrap();
            black_box(result.matches.len());
        });
    });
}

fn bench_literal_search_long(c: &mut Criterion) {
    let (index, _tmp) = setup_index(50, 5 * 1024);
    c.bench_function("literal_search_10char", |b| {
        b.iter(|| {
            let result = index
                .search_literal(black_box("compute(x_"), true, 0)
                .unwrap();
            black_box(result.matches.len());
        });
    });
}

fn bench_literal_search_brute_force(c: &mut Criterion) {
    let (index, _tmp) = setup_index(50, 5 * 1024);
    c.bench_function("literal_search_brute_2char", |b| {
        b.iter(|| {
            let result = index.search_literal(black_box("fn"), true, 0).unwrap();
            black_box(result.matches.len());
        });
    });
}

fn bench_literal_search_case_insensitive(c: &mut Criterion) {
    let (index, _tmp) = setup_index(50, 5 * 1024);
    c.bench_function("literal_search_case_insensitive", |b| {
        b.iter(|| {
            let result = index.search_literal(black_box("FUNC"), false, 0).unwrap();
            black_box(result.matches.len());
        });
    });
}

fn bench_regex_with_literals(c: &mut Criterion) {
    let (index, _tmp) = setup_index(50, 5 * 1024);
    c.bench_function("regex_func_dot_star_main", |b| {
        b.iter(|| {
            let result = index
                .search_regex(black_box("func.*main"), true, 0)
                .unwrap();
            black_box(result.matches.len());
        });
    });
}

fn bench_regex_brute_force(c: &mut Criterion) {
    let (index, _tmp) = setup_index(10, 2 * 1024);
    c.bench_function("regex_brute_force_charclass", |b| {
        b.iter(|| {
            let opts = SearchOptions {
                max_matches: 100,
                ..Default::default()
            };
            let result = index
                .search_regex_opts(black_box("[a-z]+"), &opts)
                .unwrap();
            black_box(result.matches.len());
        });
    });
}

criterion_group!(
    benches,
    bench_index_build_small,
    bench_index_build_medium,
    bench_literal_search_short,
    bench_literal_search_long,
    bench_literal_search_brute_force,
    bench_literal_search_case_insensitive,
    bench_regex_with_literals,
    bench_regex_brute_force,
);
criterion_main!(benches);
