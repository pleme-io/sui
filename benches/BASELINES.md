# sui benchmark baselines

Captured 2026-04-06 on a macOS aarch64 (M-series) laptop in debug
criterion mode with `--warm-up-time 1 --measurement-time 2
--sample-size 10`. These are rough baselines to spot obvious
regressions — re-run with default criterion settings (much longer)
before treating any single comparison as authoritative.

## sui-eval/benches/eval.rs

| bench                    | median time |
|--------------------------|-------------|
| `parse/trivial`          | ~4.4 µs     |
| `parse/let_chain`        | ~11.7 µs    |
| `parse/nested_attrs`     | ~15.8 µs    |
| `parse/long_list`        | ~13.9 µs    |
| `parse/complex_flake`    | ~24.4 µs    |
| `eval/arith`             | ~33.4 µs    |
| `eval/let_5`             | ~83.6 µs    |
| `eval/rec_fib_small`     | *measured separately* |
| `eval/list_map_20`       | *measured separately* |
| `eval/list_foldl_100`    | *measured separately* |
| `eval/attrset_merge`     | *measured separately* |
| `to_json_medium`         | *measured separately* |

## sui-compat/benches/store_path.rs

| bench                                           | median time |
|-------------------------------------------------|-------------|
| `nix_base32_encode_20`                          | ~98 ns      |
| `compress_hash_32_to_20`                        | ~55 ns      |
| `compute_store_path_from_fingerprint_typical`   | ~976 ns     |
| `compute_drv_path_typical`                      | ~4.7 µs     |

## sui-compat/benches/nar.rs

| bench                       | median time |
|-----------------------------|-------------|
| `nar_write_100_files`       | ~39 µs      |
| `nar_read_100_files`        | ~84 µs      |
| `nar_round_trip_100_files`  | ~124 µs     |

## How to refresh

```bash
cargo bench -p sui-eval    --bench eval
cargo bench -p sui-compat  --bench store_path
cargo bench -p sui-compat  --bench nar
```

Baseline numbers above are rough — capture a full run
(`--warm-up-time 3 --measurement-time 5 --sample-size 100`) before
using these as the reference for a pre/post-patch comparison.
