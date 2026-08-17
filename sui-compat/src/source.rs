//! Source-tree store-path computation — the primitive behind
//! `builtins.getFlake "path:..."`.
//!
//! CppNix, when asked for a `path:` flake ref, serializes the
//! source tree as a NAR archive (excluding `.git` by default),
//! hashes the NAR with sha256, and produces:
//!
//!   - a store path of the form `/nix/store/<hash>-source`
//!     (computed via the `fixed-output-hash` "source" branch),
//!   - a SRI-format `narHash` of the form `sha256-<base64>`.
//!
//! Both are surfaced on the flake result as `outPath` + `narHash`
//! (top level) and duplicated inside `sourceInfo`.
//!
//! This module is the single place we serialize + hash a source
//! tree.  Callers (currently just the flake evaluator in
//! sui-eval) go through one function and get both outputs
//! atomically — no chance of the hash drifting from the path.

use std::io::Cursor;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::hash::{base64_encode, HashAlgorithm, NixHash};
use crate::nar::{NarError, NarWriter};
use crate::store_path::compute_fixed_output_hash;

// ── Diagnostic perf trace (gated by SUI_PERF_TRACE=1) ──────────────
//
// Off by default (one relaxed atomic load per call when disabled).
// When enabled, records how often `nar_hash_source_tree` runs, on
// which trees, the bytes hashed, and the wall time — so we can see
// whether the SAME source tree is being NAR-hashed repeatedly (a
// memoizable storm) vs many distinct trees (an inherent cost).
//
// This changes NO value the evaluator observes: it only counts.
mod perf_trace {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    static ENABLED: AtomicBool = AtomicBool::new(false);
    static INIT: std::sync::Once = std::sync::Once::new();

    #[derive(Default)]
    pub struct Stat {
        pub calls: u64,
        pub bytes: u64,
        pub elapsed: Duration,
    }

    // path -> (call_count, total_bytes, total_elapsed)
    static PER_PATH: Mutex<Option<HashMap<String, Stat>>> = Mutex::new(None);

    #[inline]
    pub fn enabled() -> bool {
        INIT.call_once(|| {
            if std::env::var("SUI_PERF_TRACE").ok().as_deref() == Some("1") {
                ENABLED.store(true, Ordering::Relaxed);
                *PER_PATH.lock().unwrap() = Some(HashMap::new());
            }
        });
        ENABLED.load(Ordering::Relaxed)
    }

    pub fn record(path: &str, bytes: u64, elapsed: Duration) {
        if let Ok(mut g) = PER_PATH.lock() {
            if let Some(map) = g.as_mut() {
                let s = map.entry(path.to_string()).or_default();
                s.calls += 1;
                s.bytes += bytes;
                s.elapsed += elapsed;
                // Periodic progress line so a BOUNDED run (that never
                // completes) still surfaces the accumulating storm.
                let total_calls: u64 = map.values().map(|s| s.calls).sum();
                if total_calls % 200 == 0 {
                    let total_bytes: u64 = map.values().map(|s| s.bytes).sum();
                    let total_elapsed: Duration = map.values().map(|s| s.elapsed).sum();
                    let redundant: u64 =
                        map.values().map(|s| s.calls.saturating_sub(1)).sum();
                    eprintln!(
                        "[nar-trace] calls:{total_calls} distinct:{} redundant:{redundant} bytes:{:.0}MiB nar_time:{:.1}s",
                        map.len(),
                        total_bytes as f64 / 1_048_576.0,
                        total_elapsed.as_secs_f64(),
                    );
                    let mut rows: Vec<(&String, &Stat)> = map.iter().collect();
                    rows.sort_by(|a, b| b.1.elapsed.cmp(&a.1.elapsed));
                    for (p, s) in rows.iter().take(5) {
                        eprintln!(
                            "    [{:>4} calls, {:>7.1}s, {:>6.1}MiB/call] {}",
                            s.calls,
                            s.elapsed.as_secs_f64(),
                            s.bytes as f64 / 1_048_576.0 / s.calls as f64,
                            p
                        );
                    }
                }
            }
        }
    }

    /// Dump a summary to stderr. Called on process teardown via the
    /// `nar_hash_dump` helper, or explicitly by a diagnostic harness.
    pub fn dump() {
        let g = match PER_PATH.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(map) = g.as_ref() else { return };
        let mut total_calls = 0u64;
        let mut total_bytes = 0u64;
        let mut total_elapsed = Duration::ZERO;
        let mut repeated = 0u64;
        let mut rows: Vec<(&String, &Stat)> = map.iter().collect();
        for (_p, s) in &rows {
            total_calls += s.calls;
            total_bytes += s.bytes;
            total_elapsed += s.elapsed;
            if s.calls > 1 {
                repeated += s.calls - 1;
            }
        }
        rows.sort_by(|a, b| b.1.elapsed.cmp(&a.1.elapsed));
        eprintln!("\n=== nar_hash_source_tree trace ===");
        eprintln!("distinct trees: {}", map.len());
        eprintln!("total calls:    {total_calls}");
        eprintln!(
            "redundant calls (same tree >1x): {repeated}  ({:.1}%)",
            if total_calls > 0 {
                repeated as f64 / total_calls as f64 * 100.0
            } else {
                0.0
            }
        );
        eprintln!("total bytes:    {total_bytes} ({:.1} MiB)", total_bytes as f64 / 1_048_576.0);
        eprintln!("total elapsed:  {:.2}s", total_elapsed.as_secs_f64());
        eprintln!("--- top 15 trees by elapsed ---");
        for (p, s) in rows.iter().take(15) {
            eprintln!(
                "  {:>6} calls  {:>8.1} MiB  {:>7.2}s  {}",
                s.calls,
                s.bytes as f64 / 1_048_576.0,
                s.elapsed.as_secs_f64(),
                p
            );
        }
        eprintln!("==================================\n");
    }
}

/// Dump the `nar_hash_source_tree` perf trace to stderr (no-op unless
/// `SUI_PERF_TRACE=1`). Call at the end of a diagnostic eval.
pub fn nar_hash_dump() {
    if perf_trace::enabled() {
        perf_trace::dump();
    }
}

// ── Source-tree NAR-hash memo ──────────────────────────────────────
//
// A single large `nix eval` (e.g. a darwin `system.build.toplevel`)
// NAR-hashes the *same* handful of large source trees hundreds of
// times: measured on the cid marquee eval, 400 calls covered only
// ~183 distinct trees (>50% redundant) and 100% of wall-clock went
// into re-walking + re-hashing trees already hashed once (11 GiB
// re-serialized in one 210s window).
//
// The NAR hash of a directory is a pure function of its content, and
// a source tree does not change mid-eval, so a `(canonical dir, name)`
// key maps to exactly one `(store_path, nar_hash_sri)` — memoizing it
// returns the byte-IDENTICAL result while skipping the re-walk. This
// changes NO drvPath: the cached hash is the same hash the walk would
// recompute.
//
// `nar_bytes` is NOT cached (it would cost gigabytes of RAM and no
// production caller reads it — every sui-eval consumer uses only
// `store_path`/`nar_hash_sri`). A memo hit returns an empty
// `nar_bytes`; a caller that genuinely needs the archive bytes must
// call [`nar_hash_source_tree_uncached`].
mod nar_memo {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    // Keyed on `(canonical dir string, name)` → (store_path, nar_hash_sri).
    static MEMO: Mutex<Option<HashMap<(String, String), (String, String)>>> =
        Mutex::new(None);
    // Enabled by default; a single one-way `SUI_NO_NAR_MEMO=1` disables it
    // (escape hatch for parity debugging — the memo must never change a
    // value, so disabling it must be a NO-OP on every drvPath).
    static DISABLED: AtomicBool = AtomicBool::new(false);
    static INIT: std::sync::Once = std::sync::Once::new();

    fn active() -> bool {
        INIT.call_once(|| {
            if std::env::var("SUI_NO_NAR_MEMO").ok().as_deref() == Some("1") {
                DISABLED.store(true, Ordering::Relaxed);
            }
        });
        !DISABLED.load(Ordering::Relaxed)
    }

    /// A fetched flake input lives at `~/.cache/sui/inputs/<narhash>/…`: the
    /// directory name IS the content hash, so the tree is immutable by
    /// construction and its NAR hash is a pure function of a path that can
    /// never denote different bytes. Those, and only those, are safe to memoize
    /// ACROSS runs with no fingerprint, no mtime heuristic and no staleness
    /// class. A working tree gets no disk tier: it mutates under us.
    fn content_addressed(dir: &str) -> bool {
        embedded_nar_hash(dir).is_some()
    }

    /// The content hash the PATH ITSELF claims, e.g. `sha256-avzRM+ff…` from
    /// `~/.cache/sui/inputs/sha256-avzRM+ff…/bm-guard-<rev>/`.
    ///
    /// ── ★ THIS IS THE ORACLE THE MEMO WAS MISSING ─────────────────────────
    /// The doc above argues the disk tier needs "no fingerprint, no mtime
    /// heuristic and no staleness class" because the directory name IS the
    /// content hash. The first half of that is sound and the conclusion does
    /// not follow: a path that STATES its hash is not a path that has been
    /// CHECKED against it. The fetcher extracts into the final path
    /// non-atomically (and `tar` with `overwrite: true` unions a re-extraction
    /// over whatever is already there), so the tree at a content-addressed
    /// path is transiently something else — and a memo written from that
    /// window is wrong for the life of the cache.
    ///
    /// Measured 2026-08-17 on this host: the entry for
    /// `…/inputs/sha256-avzRM+ff…/bm-guard-7fd3322…` recorded
    /// `sha256-cEiI0dqw…`, so `getFlake` returned
    /// `/nix/store/w33ka5il2…-source` where `CppNix` returns
    /// `/nix/store/afw37kjkfn31…-source`. Both engines agreed on the NAR hash
    /// and on the store-path arithmetic (an audit of all 925 entries found
    /// 925 internally consistent); the single wrong input was the memo. That
    /// is a persistent, cross-process byte-parity divergence produced by a
    /// cache, which is the one thing a cache must never do.
    ///
    /// So the check costs a string compare and the path was carrying the
    /// answer the whole time. Padding-insensitive because the fetcher strips
    /// `=` when it builds the directory name.
    fn embedded_nar_hash(dir: &str) -> Option<&str> {
        let mut parts = dir.split('/').peekable();
        while let Some(p) = parts.next() {
            if p == "inputs" {
                let next = parts.peek()?;
                if next.starts_with("sha256-") || next.starts_with("sha512-") {
                    return Some(next);
                }
                return None;
            }
        }
        None
    }

    /// Whether a recorded SRI denotes the same digest as a hash embedded in a
    /// directory name.
    ///
    /// ── ★ THE DIRECTORY NAME IS A SANITIZED SRI, NOT AN SRI ───────────────
    /// The fetcher builds the directory with
    /// `hash.replace(':', "-").replace('/', "_").replace('=', "")`
    /// (`sui-eval/src/fetcher.rs:345`), so base64's `/` becomes `_` and the
    /// padding is dropped. Comparing the two forms literally is not a stricter
    /// check, it is a WRONG one.
    ///
    /// Measured before this function existed: a literal comparison flagged
    /// **116 of 238** live cache entries as poisoned — every input whose
    /// base64 digest happens to contain a `/`. Shipping that would have
    /// deleted 116 valid records on read and, via the write-side guard,
    /// permanently refused to memoize that whole class. A verification pass
    /// that condemns half the corpus is reporting its own bug, not the
    /// corpus's.
    fn sri_matches_dir_hash(recorded_sri: &str, dir_hash: &str) -> bool {
        sanitize_hash(recorded_sri) == sanitize_hash(dir_hash)
    }

    /// Mirror of the fetcher's directory-name sanitizer. Applied to BOTH
    /// sides so the comparison is well-defined whichever form each carries.
    fn sanitize_hash(h: &str) -> String {
        h.replace(':', "-").replace('/', "_").replace('=', "")
    }

    fn disk_entry(dir: &str, name: &str) -> Option<std::path::PathBuf> {
        use sha2::Digest;
        let home = std::env::var_os("HOME")?;
        let mut h = sha2::Sha256::new();
        h.update(dir.as_bytes());
        h.update(b"\0");
        h.update(name.as_bytes());
        let key = format!("{:x}", h.finalize());
        Some(
            std::path::Path::new(&home)
                .join(".cache/sui/nar-memo")
                .join(key),
        )
    }

    fn disk_get(dir: &str, name: &str) -> Option<(String, String)> {
        let path = disk_entry(dir, name)?;
        let body = std::fs::read_to_string(&path).ok()?;
        let (store_path, nar_hash_sri) = body.split_once('\n')?;
        if store_path.is_empty() || nar_hash_sri.is_empty() {
            return None;
        }
        let nar_hash_sri = nar_hash_sri.trim_end();

        // ── ★ VERIFY THE ENTRY AGAINST THE PATH'S OWN CLAIM ───────────────
        // The NAR hash is a function of the TREE, not of `name` — `name` only
        // feeds the store-path fingerprint — so a content-addressed path's
        // embedded hash is the right oracle for every entry under it,
        // whatever it is named.
        //
        // A mismatch is not a miss to paper over: it is a poisoned record
        // that would otherwise be returned forever, so DELETE it and force a
        // recompute. That makes the failure self-healing rather than
        // permanent, which matters because the poisoning window (a partial or
        // unioned extraction) is invisible after the fact.
        if let Some(expected) = embedded_nar_hash(dir)
            && !sri_matches_dir_hash(nar_hash_sri, expected)
        {
            let _ = std::fs::remove_file(&path);
            return None;
        }

        Some((store_path.to_string(), nar_hash_sri.to_string()))
    }

    /// Best effort: a failed write costs hit rate, never correctness. One file
    /// per entry written via tmp+rename, so concurrent evals cannot interleave
    /// a torn record.
    fn disk_put(dir: &str, name: &str, store_path: &str, nar_hash_sri: &str) {
        // Refuse to persist a record the path itself contradicts. This is the
        // write-side half of the same invariant `disk_get` enforces on read:
        // catching it here means a tree hashed during a partial extraction
        // never reaches disk at all, so the poisoning window closes instead of
        // merely self-healing on the next read.
        if let Some(expected) = embedded_nar_hash(dir)
            && !sri_matches_dir_hash(nar_hash_sri, expected)
        {
            return;
        }
        let Some(path) = disk_entry(dir, name) else {
            return;
        };
        let Some(parent) = path.parent() else { return };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, format!("{store_path}\n{nar_hash_sri}\n")).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    pub fn get(dir: &str, name: &str) -> Option<(String, String)> {
        if !active() {
            return None;
        }
        {
            let g = MEMO.lock().ok()?;
            if let Some(hit) = g
                .as_ref()
                .and_then(|m| m.get(&(dir.to_string(), name.to_string())))
                .cloned()
            {
                return Some(hit);
            }
        }
        if !content_addressed(dir) {
            return None;
        }
        let hit = disk_get(dir, name)?;
        if let Ok(mut g) = MEMO.lock() {
            g.get_or_insert_with(HashMap::new)
                .insert((dir.to_string(), name.to_string()), hit.clone());
        }
        Some(hit)
    }

    pub fn put(dir: &str, name: &str, store_path: String, nar_hash_sri: String) {
        if !active() {
            return;
        }
        if content_addressed(dir) {
            disk_put(dir, name, &store_path, &nar_hash_sri);
        }
        if let Ok(mut g) = MEMO.lock() {
            g.get_or_insert_with(HashMap::new)
                .insert((dir.to_string(), name.to_string()), (store_path, nar_hash_sri));
        }
    }

    #[cfg(test)]
    mod content_addressed_tests {
        use super::content_addressed;

        #[test]
        fn a_fetched_input_is_content_addressed() {
            assert!(content_addressed(
                "/Users/x/.cache/sui/inputs/sha256-KoTsyMQqnXQ/nixpkgs-148bab9"
            ));
        }

        #[test]
        fn a_working_tree_is_not() {
            assert!(
                !content_addressed("/Users/x/code/github/example-org/example-images"),
                "a working tree mutates, so memoizing it across runs would serve bytes that no \
                 longer exist -- exactly the stale-source class the disk tier must never enter"
            );
        }

        #[test]
        fn an_inputs_dir_without_a_hash_segment_is_not() {
            assert!(!content_addressed("/Users/x/.cache/sui/inputs/scratch"));
        }
    }

    #[cfg(test)]
    mod poisoned_memo_tests {
        use super::{disk_get, disk_put, embedded_nar_hash, sri_matches_dir_hash};

        // The exact record found poisoned on this host, 2026-08-17.
        const DIR: &str = "/Users/x/.cache/sui/inputs/\
                           sha256-avzRM+ffKgikqMRcOhhYp3ifgwXMGbH0rEGEZPEGMYE/bm-guard-7fd3322";
        const TRUE_SRI: &str = "sha256-avzRM+ffKgikqMRcOhhYp3ifgwXMGbH0rEGEZPEGMYE=";
        const WRONG_SRI: &str = "sha256-cEiI0dqw2Tktwj7tx6f0yJZktBan/hcrMVVRzuBCDgs=";
        const WRONG_PATH: &str = "/nix/store/w33ka5il2fhxp2vzfdbjwmla3szfgc95-source";
        const TRUE_PATH: &str = "/nix/store/afw37kjkfn31rfccgbbdr2rnkbiws5ms-source";

        #[test]
        fn the_path_states_its_own_hash_and_padding_does_not_matter() {
            // The fetcher strips `=` building the directory name, so the
            // comparison must be padding-insensitive or every entry looks
            // poisoned and the memo silently degrades to a 0% hit rate.
            let e = embedded_nar_hash(DIR).expect("a fetched input states its hash");
            assert_eq!(e, "sha256-avzRM+ffKgikqMRcOhhYp3ifgwXMGbH0rEGEZPEGMYE");
            assert!(
                sri_matches_dir_hash(TRUE_SRI, e),
                "padding AND the / -> _ substitution must not break the match"
            );
            assert!(!sri_matches_dir_hash(WRONG_SRI, e));
            assert_eq!(embedded_nar_hash("/Users/x/code/some-worktree"), None);
        }

        /// ★ THE REGRESSION. Before this change `disk_get` returned whatever
        /// was on disk, so a record written during a partial extraction served
        /// a wrong `outPath` for the life of the cache — persistently, across
        /// processes, on an engine whose entire thesis is byte-parity.
        #[test]
        fn a_poisoned_entry_is_refused_and_deleted_rather_than_served() {
            let home = tempfile::tempdir().expect("tmp home");
            temp_env_home(home.path(), || {
                // Plant exactly the poisoned shape: a store path that follows
                // correctly from a hash which is NOT this tree's.
                disk_put_raw(DIR, "source", WRONG_PATH, WRONG_SRI);
                assert!(
                    disk_get(DIR, "source").is_none(),
                    "a record contradicting the path's own hash must never be served"
                );
                assert!(
                    disk_get(DIR, "source").is_none(),
                    "and it must be GONE, so the failure self-heals instead of persisting"
                );

                // The honest counterpart: a correct record still round-trips,
                // or this 'fix' would just be a cache that never hits.
                disk_put(DIR, "source", TRUE_PATH, TRUE_SRI);
                assert_eq!(
                    disk_get(DIR, "source"),
                    Some((TRUE_PATH.to_string(), TRUE_SRI.to_string())),
                    "a consistent record must still be served"
                );
            });
        }

        #[test]
        fn a_contradicting_record_is_never_written_in_the_first_place() {
            let home = tempfile::tempdir().expect("tmp home");
            temp_env_home(home.path(), || {
                disk_put(DIR, "source", WRONG_PATH, WRONG_SRI);
                assert!(
                    disk_get(DIR, "source").is_none(),
                    "the write side must close the poisoning window, not just the read side"
                );
            });
        }

        /// ★ THE NEAR-MISS THIS TEST EXISTS FOR.
        ///
        /// The first version of the verifier compared the recorded SRI to the
        /// directory name with only `=` padding stripped. That is wrong: the
        /// fetcher also maps `/` → `_` and `:` → `-` building the name, so
        /// every input whose base64 digest contains a `/` looked poisoned.
        /// Measured against the live cache before this test existed: **116 of
        /// 238** entries would have been deleted on read and then refused on
        /// write — silently disabling the disk memo for that entire class.
        ///
        /// A digest containing `/` is not an edge case; it is ~49% of them.
        #[test]
        fn a_digest_containing_a_slash_is_not_mistaken_for_poison() {
            // Real shape from the live cache: base64 `/`, directory `_`.
            let sri = "sha256-C6uP9917/Rtz3B1VM9KrZjVMHsjy6Jjatp9N6oO4ycI=";
            let dir_hash = "sha256-C6uP9917_Rtz3B1VM9KrZjVMHsjy6Jjatp9N6oO4ycI";
            assert!(
                sri_matches_dir_hash(sri, dir_hash),
                "`/` in base64 becomes `_` in the directory name — comparing the \
                 two forms literally condemns half the corpus"
            );
            // And `:` → `-`, the third substitution.
            assert!(sri_matches_dir_hash("sha256:abc=", "sha256-abc"));
            // A genuinely different digest must still be caught.
            assert!(!sri_matches_dir_hash(
                "sha256-cEiI0dqw2Tktwj7tx6f0yJZktBan/hcrMVVRzuBCDgs=",
                dir_hash
            ));
        }

        #[test]
        fn a_non_content_addressed_path_is_still_memoized_unverified() {
            // A worktree states no hash, so there is nothing to verify
            // against. It must keep working — `content_addressed` already
            // keeps it off the DISK tier, and that is the existing contract.
            let home = tempfile::tempdir().expect("tmp home");
            temp_env_home(home.path(), || {
                let wt = "/Users/x/code/some-worktree";
                disk_put(wt, "source", TRUE_PATH, TRUE_SRI);
                assert_eq!(
                    disk_get(wt, "source"),
                    Some((TRUE_PATH.to_string(), TRUE_SRI.to_string())),
                    "no embedded hash means no verification, not a refusal"
                );
            });
        }

        /// Write without going through `disk_put`'s validation, so the
        /// poisoned state can be planted at all.
        fn disk_put_raw(dir: &str, name: &str, store_path: &str, sri: &str) {
            let path = super::disk_entry(dir, name).expect("entry path");
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, [store_path, "\n", sri, "\n"].concat()).expect("write");
        }

        /// `HOME` is process-global, so these tests must not run concurrently
        /// with each other; one mutex serialises them.
        fn temp_env_home(home: &std::path::Path, f: impl FnOnce()) {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let _g = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let prev = std::env::var_os("HOME");
            // SAFETY: serialised by LOCK; no other thread reads HOME here.
            unsafe { std::env::set_var("HOME", home) };
            f();
            match prev {
                Some(p) => unsafe { std::env::set_var("HOME", p) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }
}

/// Result of serializing + hashing a source tree.
#[derive(Debug, Clone)]
pub struct SourceHash {
    /// Store path the source would be materialized under, e.g.
    /// `/nix/store/p8zn7x0860a3h5xf1dg01a3sfxs3s46i-source`.
    pub store_path: String,
    /// SRI-format NAR hash, e.g.
    /// `sha256-fpA5m7tc6t4Oe6Uku9gKvul7CrR7urWE1K+DA0nhLPI=`.
    /// This is what CppNix exposes as the `narHash` attribute on
    /// flake results.
    pub nar_hash_sri: String,
    /// Raw NAR bytes.  Callers that want to cache or upload the
    /// archive (binary cache push, store materialization) use this
    /// directly — re-serializing would be both wasteful and risks
    /// nondeterminism.
    pub nar_bytes: Vec<u8>,
}

/// NAR-serialize `dir`, hash it, and compute the CppNix source
/// store path + SRI narHash.
///
/// The `name` argument is the final `-<name>` segment of the
/// resulting store path.  For flake `path:` refs CppNix uses
/// `"source"` unconditionally.
///
/// **Memoized.** A `(dir, name)` key that was hashed before returns the
/// cached `(store_path, nar_hash_sri)` — byte-identical to a fresh walk
/// (a source tree does not change mid-eval, so the NAR hash is a pure
/// function of the tree content). The re-walk is skipped. On a memo hit
/// `nar_bytes` is EMPTY (no production caller reads it; caching gigabytes
/// of archive bytes would be wasteful). A caller that needs the archive
/// bytes must use [`nar_hash_source_tree_uncached`].
///
/// # Errors
///
/// Returns a [`NarError`] if the path can't be serialized (e.g.
/// broken symlink, unreadable directory).
pub fn nar_hash_source_tree(dir: &Path, name: &str) -> Result<SourceHash, NarError> {
    let dir_key = dir.to_string_lossy();
    if let Some((store_path, nar_hash_sri)) = nar_memo::get(&dir_key, name) {
        return Ok(SourceHash {
            store_path,
            nar_hash_sri,
            // Byte-identical hashes; the archive bytes are omitted on a
            // memo hit (documented above). No sui-eval consumer reads them.
            nar_bytes: Vec::new(),
        });
    }

    let sh = nar_hash_source_tree_uncached(dir, name)?;
    nar_memo::put(
        &dir_key,
        name,
        sh.store_path.clone(),
        sh.nar_hash_sri.clone(),
    );
    Ok(sh)
}

/// The uncached NAR-serialize + hash. Always walks the tree and returns
/// the full [`SourceHash`] including `nar_bytes`. [`nar_hash_source_tree`]
/// is the memoized wrapper over this; call this directly only when you
/// genuinely need the archive bytes on every call.
///
/// # Errors
///
/// Returns a [`NarError`] if the path can't be serialized.
pub fn nar_hash_source_tree_uncached(dir: &Path, name: &str) -> Result<SourceHash, NarError> {
    let trace = perf_trace::enabled();
    let t0 = if trace { Some(std::time::Instant::now()) } else { None };

    let mut nar_bytes = Vec::new();
    {
        let mut cursor = Cursor::new(&mut nar_bytes);
        NarWriter::write_path(&mut cursor, dir)?;
    }

    if let Some(t0) = t0 {
        perf_trace::record(&dir.to_string_lossy(), nar_bytes.len() as u64, t0.elapsed());
    }

    // Inner sha256 of the NAR, in lowercase hex — fed to
    // `compute_fixed_output_hash` which expects hex.
    let digest = Sha256::digest(&nar_bytes);
    let digest_bytes = digest.to_vec();
    let hex: String = digest_bytes.iter().map(|b| format!("{b:02x}")).collect();

    let store_path = compute_fixed_output_hash("sha256", &hex, true, name);

    // SRI = `sha256-<base64>` over the RAW digest bytes (not the hex).
    let nar_hash = NixHash::new(HashAlgorithm::Sha256, digest_bytes.clone());
    let nar_hash_sri = nar_hash.to_sri();

    Ok(SourceHash {
        store_path,
        nar_hash_sri,
        nar_bytes,
    })
}

/// Base64 encode the SHA-256 of `bytes` without the `sha256-`
/// prefix.  Exposed for callers that already have NAR bytes in
/// hand (e.g. a cache hit).
#[must_use]
pub fn base64_sha256(bytes: &[u8]) -> String {
    base64_encode(&Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn mk_flake_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let flake_nix = dir.path().join("flake.nix");
        let mut f = std::fs::File::create(&flake_nix).unwrap();
        // Exact bytes we probed against CppNix.
        write!(f, "{{ outputs = {{ self }}: {{ value = 42; }}; }}\n").unwrap();
        dir
    }

    #[test]
    fn source_tree_produces_a_store_path_and_an_sri_hash() {
        let dir = mk_flake_dir();
        let sh = nar_hash_source_tree(dir.path(), "source").expect("nar hash");
        // Structural assertions — any NAR-hash-of-a-tree must have
        // these shapes.  The exact CppNix parity is asserted in an
        // integration test (requires nix binary).
        assert!(sh.store_path.starts_with("/nix/store/"));
        assert!(sh.store_path.ends_with("-source"));
        assert!(sh.nar_hash_sri.starts_with("sha256-"));
        assert!(!sh.nar_bytes.is_empty());
        assert!(sh.nar_bytes.starts_with(b"\r\x00\x00\x00\x00\x00\x00\x00nix-archive-1"),
            "NAR must begin with the magic header — got {:?}",
            &sh.nar_bytes[..16]);
    }

    #[test]
    fn memo_returns_byte_identical_hashes_on_repeat() {
        // First call (miss) walks + hashes; second call (hit) returns the
        // cached hashes. The store_path + SRI hash MUST be byte-identical
        // — the memo is a pure-function cache, never a value change.
        let dir = mk_flake_dir();
        let first = nar_hash_source_tree(dir.path(), "source").expect("first");
        let second = nar_hash_source_tree(dir.path(), "source").expect("second");
        assert_eq!(first.store_path, second.store_path, "memo changed store_path");
        assert_eq!(first.nar_hash_sri, second.nar_hash_sri, "memo changed narHash");

        // The uncached path recomputes the SAME hashes as the memo — proof
        // the memo doesn't drift from a fresh walk.
        let uncached = nar_hash_source_tree_uncached(dir.path(), "source").expect("uncached");
        assert_eq!(first.store_path, uncached.store_path);
        assert_eq!(first.nar_hash_sri, uncached.nar_hash_sri);
        assert!(!uncached.nar_bytes.is_empty(), "uncached always yields NAR bytes");
    }

    #[test]
    fn memo_keys_on_name_not_just_dir() {
        // Same dir, different `name` → different store path (the name is
        // the `-<name>` suffix). The memo must not collide the two keys.
        let dir = mk_flake_dir();
        let a = nar_hash_source_tree(dir.path(), "source").expect("a");
        let b = nar_hash_source_tree(dir.path(), "other").expect("b");
        assert_ne!(a.store_path, b.store_path, "name must be part of the memo key");
        // Same content → same NAR hash regardless of name.
        assert_eq!(a.nar_hash_sri, b.nar_hash_sri);
    }
}

/// Strip a leading `<32-char nix-base32 hash>-` from a store-path basename.
///
/// `/nix/store/<hash>-foo` → `foo`; anything else is returned unchanged.
///
/// ── ★ WHY THIS EXISTS ────────────────────────────────────────────────────
/// A path already inside the store has a basename of `<hash>-<name>`. Passing
/// that to [`nar_hash_source_tree`] as the NAME yields
/// `<newhash>-<oldhash>-<name>` — a store path carrying the old hash inside it.
/// CppNix's `addToStore` takes the name as a separate argument and never
/// re-derives it from a basename, so nix is unaffected.
///
/// MEASURED 2026-08-11: this produced sui's only divergence in `minimal`'s
/// 189-path system closure (`nixos-firewall-tool`, whose `src` is a
/// `filterSource` over an in-store directory), 66 extra ATerm bytes → a
/// different `out` → a different drvPath → a different node toplevel.
///
/// The nix-base32 alphabet omits e/o/u/t, so a 32-char run of it followed by
/// `-` is unambiguous: a real name of that shape would have to avoid five
/// letters across all 32 positions.
#[must_use]
pub fn strip_store_hash_prefix(name: &str) -> &str {
    const NIX_BASE32: &str = "0123456789abcdfghijklmnpqrsvwxyz";
    match name.as_bytes().get(32) {
        Some(b'-') if name.is_char_boundary(32) && name[..32].chars().all(|c| NIX_BASE32.contains(c)) => &name[33..],
        _ => name,
    }
}

#[cfg(test)]
mod strip_store_hash_prefix_tests {
    use super::strip_store_hash_prefix;

    #[test]
    fn a_store_basename_loses_exactly_its_hash() {
        // The measured 2026-08-11 case: `filterSource` over an in-store dir.
        // Without the strip the copy landed at `<newhash>-<oldhash>-<name>`,
        // which changed `out`, the drvPath, and the whole node toplevel.
        assert_eq!(
            strip_store_hash_prefix("ar6s9jl94xw7fvvzy8p1hn6635i20bl2-nixos-firewall-tool"),
            "nixos-firewall-tool"
        );
    }

    #[test]
    fn a_plain_name_is_untouched() {
        for n in ["source", "nixos-firewall-tool", "hello-2.12.2", ""] {
            assert_eq!(strip_store_hash_prefix(n), n, "must not rewrite {n:?}");
        }
    }

    #[test]
    fn a_lookalike_that_is_not_a_hash_is_untouched() {
        // 32 chars + '-', but 'e','o','u','t' are NOT in nix-base32, so this is
        // a package name and stripping it would silently corrupt the store path.
        let name = "eoutbase32lookalikeeoutbase32look-thing";
        assert_eq!(strip_store_hash_prefix(name), name);
        // 31 chars + '-' is also not a hash.
        assert_eq!(strip_store_hash_prefix("0123456789abcdfghijklmnpqrsvwxy-x"),
                   "0123456789abcdfghijklmnpqrsvwxy-x");
    }

    #[test]
    fn a_multibyte_name_does_not_panic() {
        // `is_char_boundary` guard: slicing [..32] on a non-boundary would panic.
        let name = "日本語のとても長いパッケージ名前ですここまで-x";
        assert_eq!(strip_store_hash_prefix(name), name);
    }
}
