//! Criterion benchmarks for sui-compat store path + hash utilities.
//!
//! Baselines (capture once, compare on each subsequent run):
//!
//! - nix_base32_encode(&[u8; 20])
//! - compress_hash(&[u8; 32], 20)
//! - compute_store_path_from_fingerprint(<typical-length>, <name>)
//! - compute_drv_path over a realistic .drv content blob
//!
//! Run with: `cargo bench -p sui-compat --bench store_path`.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use sui_compat::store_path::{
    compress_hash, compute_drv_path, compute_store_path_from_fingerprint, nix_base32_encode,
};

fn bench_base32(c: &mut Criterion) {
    let digest: [u8; 20] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70,
        0x80, 0x90, 0xa0, 0xb0, 0xc0,
    ];
    c.bench_function("nix_base32_encode_20", |b| {
        b.iter(|| {
            let s = nix_base32_encode(black_box(&digest));
            black_box(s);
        });
    });
}

fn bench_compress(c: &mut Criterion) {
    let hash: [u8; 32] = [0u8; 32];
    c.bench_function("compress_hash_32_to_20", |b| {
        b.iter(|| {
            let c = compress_hash(black_box(&hash), 20);
            black_box(c);
        });
    });
}

fn bench_fingerprint(c: &mut Criterion) {
    let fp =
        "text:sha256:0000000000000000000000000000000000000000000000000000000000000000:/nix/store:hello-1.0.drv";
    let name = "hello-1.0.drv";
    c.bench_function("compute_store_path_from_fingerprint_typical", |b| {
        b.iter(|| {
            let p = compute_store_path_from_fingerprint(black_box(fp), black_box(name));
            black_box(p);
        });
    });
}

fn bench_drv_path(c: &mut Criterion) {
    let drv = concat!(
        r#"Derive([("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello","","")],"#,
        r#"[("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-bash.drv",["out"])],"#,
        r#"["/nix/store/cccccccccccccccccccccccccccccccc-builder.sh"],"#,
        r#""x86_64-linux","/bin/sh",["-e","/nix/store/cccccccccccccccccccccccccccccccc-builder.sh"],"#,
        r#"[("name","hello"),("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello"),("system","x86_64-linux")])"#,
    );
    c.bench_function("compute_drv_path_typical", |b| {
        b.iter(|| {
            let p = compute_drv_path(black_box(drv.as_bytes()), black_box("hello"));
            black_box(p);
        });
    });
}

criterion_group!(benches, bench_base32, bench_compress, bench_fingerprint, bench_drv_path);
criterion_main!(benches);
