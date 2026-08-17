//! Every bridge crossing must be counted — enforced at the call site, not
//! assumed.
//!
//! # Why
//!
//! `fallback::record(Layer::Builtin, …)` was added at ONE of the three places
//! that call `bridge::call_builtin_bridge`. The other two (`vm.rs`'s
//! `try_vm_builtin` arm and its regex arm) crossed to the tree-walker without
//! incrementing anything.
//!
//! That is not a cosmetic undercount. `try_vm_builtin` is consulted BEFORE the
//! builtin registry, so for every name in its arm — `readDir`, `fromTOML`,
//! `genericClosure`, `getContext`, `toXML`, `parseDrvName`, `path`,
//! `parseFlakeRef`, `hashFile`, `findFile`, … plus `match`/`split` — the
//! counted registry path is shadowed and dead. The counter's entire reachable
//! surface was `fetchClosure`, `outputOf` and `hashString`: none of the names
//! `fallback.rs`'s own module doc cites as the reason the counter exists.
//!
//! ★ It produced a WRONG CONCLUSION, not just a low number. Measuring
//! `builtins.match` and `builtins.fromTOML` at `builtin=0` was read as "they
//! became native and the comments calling them bridged had aged". They had not.
//! They were bridged the whole time, through the path nobody counted. A blind
//! instrument does not report "unknown" — it reports zero, which looks like an
//! answer.
//!
//! TIER: CI-caught (a source scan in a test). The unrepresentable form is to
//! make `call_builtin_bridge` private and expose only a `record`-ing wrapper —
//! worth doing, and named rather than scheduled.

use std::path::PathBuf;

/// Files that may call the raw bridge, with the reason.
fn is_permitted(rel: &str) -> bool {
    // The bridge module DEFINES it; `lib.rs` re-exports it.
    rel.ends_with("src/bridge.rs") || rel.ends_with("src/lib.rs")
        // This guard names it in string literals.
        || rel.ends_with("tests/fallback_counter_completeness.rs")
}

/// `import_via_bridge` is layer 2, recorded by its CALLER before the call —
/// so the crossing is counted, just not on the adjacent line.
const LAYER_TWO_FN: &str = "fn import_via_bridge";

#[test]
fn every_bridge_call_site_records_a_crossing() {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.push("src");

    let mut sites = 0usize;
    let mut unrecorded: Vec<String> = Vec::new();

    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            let rel = p.to_string_lossy().to_string();
            if is_permitted(&rel) {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&p) else {
                continue;
            };
            let lines: Vec<&str> = src.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if !line.contains("call_builtin_bridge(") {
                    continue;
                }
                sites += 1;
                // Look BACK a short window for the record. Forward would be
                // wrong: by then the crossing has already happened, and a
                // record after the call cannot fire when the call diverges.
                let lo = i.saturating_sub(12);
                let recorded = lines[lo..=i].iter().any(|l| l.contains("fallback::record"));
                // …or the enclosing fn is the layer-2 helper, whose caller records.
                let in_layer_two = lines[..=i]
                    .iter()
                    .rev()
                    .take(60)
                    .any(|l| l.contains(LAYER_TWO_FN));
                if !recorded && !in_layer_two {
                    unrecorded.push(format!("  {}:{}", rel, i + 1));
                }
            }
        }
    }

    // ANTI-VACUITY, before the verdict, carrying its own denominator. If the
    // scan stops finding call sites — a rename, a broken walk, a widened
    // permit list — then "nothing unrecorded" is the empty set passing.
    assert!(
        sites >= 3,
        "found only {sites} `call_builtin_bridge` call sites outside the \
         bridge module; expected at least 3. The scan is broken, and a clean \
         result over a broken scan is not a clean result."
    );

    assert!(
        unrecorded.is_empty(),
        "\nthese cross to the tree-walker WITHOUT counting it:\n{}\n\n\
         A bridge crossing that is not recorded makes the counter report zero \
         where the truth is unknown — which reads as an answer. That is how \
         `match` and `fromTOML` were concluded to be native when they were \
         bridged all along.\n({sites} call sites scanned)",
        unrecorded.join("\n")
    );
}
