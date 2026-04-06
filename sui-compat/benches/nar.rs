//! Criterion benchmarks for sui-compat's NAR encoder and decoder.
//!
//! Builds a synthetic NAR tree of moderate size (a directory with
//! 100 regular files and a nested subdirectory) and measures:
//!
//! - NarWriter::write time
//! - NarReader::read_complete time
//! - Round-trip time (write + read) on the same tree
//!
//! Run with: `cargo bench -p sui-compat --bench nar`.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use sui_compat::nar::{NarEntry, NarNode, NarReader, NarWriter};

fn make_tree() -> NarNode {
    let regular_file = |name: &str, contents: Vec<u8>| NarEntry {
        name: name.to_string(),
        node: NarNode::Regular {
            executable: false,
            contents,
        },
    };

    let mut entries = Vec::new();
    for i in 0..50 {
        entries.push(regular_file(
            &format!("file-{i:03}.txt"),
            format!("contents of file {i}").into_bytes(),
        ));
    }

    // nested subdirectory with another 50 files
    let mut sub_entries = Vec::new();
    for i in 0..50 {
        sub_entries.push(regular_file(
            &format!("sub-{i:03}.txt"),
            format!("sub file {i}").into_bytes(),
        ));
    }
    entries.push(NarEntry {
        name: "nested".to_string(),
        node: NarNode::Directory {
            entries: sub_entries,
        },
    });

    // Directory entries must be sorted by name for NAR spec
    // compliance — our writer trusts the input order.
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    NarNode::Directory { entries }
}

fn bench_write(c: &mut Criterion) {
    let tree = make_tree();
    c.bench_function("nar_write_100_files", |b| {
        b.iter(|| {
            let mut buf = Vec::with_capacity(8192);
            NarWriter::write(&mut buf, black_box(&tree)).expect("write");
            black_box(buf);
        });
    });
}

fn bench_read(c: &mut Criterion) {
    let tree = make_tree();
    let mut bytes = Vec::with_capacity(8192);
    NarWriter::write(&mut bytes, &tree).unwrap();

    c.bench_function("nar_read_100_files", |b| {
        b.iter(|| {
            let mut cursor = std::io::Cursor::new(black_box(bytes.as_slice()));
            let parsed = NarReader::read_complete(&mut cursor).expect("read");
            black_box(parsed);
        });
    });
}

fn bench_round_trip(c: &mut Criterion) {
    let tree = make_tree();
    c.bench_function("nar_round_trip_100_files", |b| {
        b.iter(|| {
            let mut buf = Vec::with_capacity(8192);
            NarWriter::write(&mut buf, black_box(&tree)).expect("write");
            let mut cursor = std::io::Cursor::new(buf.as_slice());
            let parsed = NarReader::read_complete(&mut cursor).expect("read");
            black_box(parsed);
        });
    });
}

criterion_group!(benches, bench_write, bench_read, bench_round_trip);
criterion_main!(benches);
