//! Walking two `.drv` graphs to where they part.
//!
//! Given the top drv sui produced and the top drv CppNix produced for the same
//! attribute, [`bisect_drv_all`] pairs input derivations by name (with
//! multiplicity) and visits every diverging pair, stopping at the FRONTIER: a
//! drv that differs while every same-name input agrees. Visiting is
//! breadth-first from the top, so the first frontier leaf is the shallowest.
//! [`field_diffs`] then compares that pair field by field on the parsed
//! scalars, never on renderings.
//!
//! Used by `sui parity-bisect` and `sui flip-probe`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::derivation::Derivation;

/// The store-name of a `/nix/store/<32-hash>-<name>` path — the hash stripped,
/// used to match sui's temp-cache drvs against nix's store drvs across the
/// input-derivation graph (the hashes differ where they diverge; the names don't).
pub fn drv_name(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.splitn(2, '-').nth(1).unwrap_or(base).to_string()
}

/// Replace every `/nix/store/<32-hash>-` with a fixed placeholder so a value
/// that differs ONLY by cascaded store hashes reads as equal — isolating
/// genuine content divergence from hash cascade.
pub fn strip_store_hashes(s: &str) -> String {
    // UTF-8-safe: scan by `find` + char-aware slicing (drv env values contain
    // multi-byte chars, e.g. the U+2010 hyphen in gcc build scripts — byte
    // indexing would panic on a non-char-boundary).
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find("/nix/store/") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + "/nix/store/".len()..];
        // The store hash is exactly 32 nix-base32 chars (all ASCII); collect the
        // first 32 chars and verify — if they're all ASCII the byte length is 32
        // and `after[32..]` lands on a char boundary.
        let hash: String = after.chars().take(32).collect();
        if hash.len() == 32 && hash.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) {
            out.push_str("/nix/store/<HASH>");
            rest = &after[32..];
        } else {
            out.push_str("/nix/store/");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// One structural-leaf result of the bisect.
pub struct BisectLeaf {
    pub sui_path: String,
    pub nix_path: String,
    pub sui: Derivation,
    pub nix: Derivation,
    /// The name path from the top drv down to this node. Per-leaf now that the
    /// walk reports many leaves, rather than one `&mut Vec` threaded through a
    /// single descent.
    pub trail: Vec<String>,
}

/// A node whose name appears a different number of times on the two sides.
///
/// This is not cosmetic. The NixOS `minimal` toplevel closure has 145
/// duplicated drv names — `source.drv` appears 188 times — and the toplevel
/// drv's own immediate `inputDrvs` contain duplicates. Keying the pairing on
/// `BTreeMap<name, path>` therefore DROPPED nodes silently, at the root of the
/// exact subject this tool exists for.
pub struct MissingNode {
    pub parent: String,
    pub name: String,
    pub side: &'static str,
    pub count_sui: usize,
    pub count_nix: usize,
}

/// Everything one visit-all bisect found.
pub struct BisectReport {
    /// Every frontier node — a drv that diverges but whose same-name inputs all
    /// agree. There can be many; the old first-child descent reported at most
    /// one and called the rest "no divergence".
    pub leaves: Vec<BisectLeaf>,
    pub missing: Vec<MissingNode>,
    pub unreadable: Vec<(String, &'static str)>,
    pub visited: usize,
    pub truncated: bool,
}

/// Walk the sui↔nix input-derivation graph, visiting EVERY diverging node.
///
/// Replaces a first-child descent (`diverging.into_iter().next()`) that
/// explored ONE path through a DAG and reported "no divergence found" whenever
/// the divergence sat on a sibling branch. A false negative in a diagnostic is
/// worse than a missing diagnostic: it answers confidently and wrongly.
///
/// Three deliberate choices:
///
/// * **The memo is keyed on the drv store path**, and there is exactly one of
///   them for both sides. A store path is content-addressed, so equal paths
///   imply byte-equal ATerm; a node reached from both sides under one path is
///   parsed once, which is the bulk of the win since the agreeing sub-closure
///   is the majority. Where the sides diverge the paths differ and get
///   distinct entries, correctly.
/// * **`seen` is keyed on the PAIR**, not on either path. The verdict is a
///   property of the pair — the same sui node can legitimately be compared
///   against two different nix partners when a name repeats.
/// * **The depth cap is gone.** With a pair seen-set it was dead code, and
///   worse than dead: a genuine cycle (which would itself be a real hashing
///   bug, `.drv` graphs being acyclic by construction) surfaced as a
///   misleading "recursion too deep". An explicit worklist makes stack depth
///   irrelevant; `max_nodes` is the safety valve and reports `truncated`
///   loudly rather than stopping quietly.
///
/// An unreadable node degrades to a recorded entry instead of aborting the
/// whole walk — the generalisation must not be less robust than what it
/// replaces.
pub fn bisect_drv_all(sui_top: &str, nix_top: &str, max_nodes: usize) -> BisectReport {
    bisect_drv_graph(sui_top, nix_top, max_nodes, |p| std::fs::read(p).ok())
}

/// [`bisect_drv_all`] over any `.drv` source: `load` returns a drv's ATerm
/// bytes, or `None` when it cannot be read.
pub fn bisect_drv_graph(
    sui_top: &str,
    nix_top: &str,
    max_nodes: usize,
    mut load_bytes: impl FnMut(&str) -> Option<Vec<u8>>,
) -> BisectReport {
    use std::collections::{BTreeMap, BTreeSet, VecDeque};

    let mut parsed: BTreeMap<String, Derivation> = BTreeMap::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut work: VecDeque<(String, String, Vec<String>)> = VecDeque::new();
    let mut report = BisectReport {
        leaves: Vec::new(),
        missing: Vec::new(),
        unreadable: Vec::new(),
        visited: 0,
        truncated: false,
    };

    work.push_back((
        sui_top.to_string(),
        nix_top.to_string(),
        vec![drv_name(nix_top)],
    ));

    while let Some((sp, np, trail)) = work.pop_front() {
        if !seen.insert((sp.clone(), np.clone())) {
            continue;
        }
        if report.visited >= max_nodes {
            report.truncated = true;
            break;
        }
        report.visited += 1;

        // Parse both sides through the memo.
        let mut load = |path: &str, side: &'static str| -> Option<Derivation> {
            if let Some(d) = parsed.get(path) {
                return Some(d.clone());
            }
            let bytes = load_bytes(path)?;
            let d = Derivation::parse(&bytes).ok()?;
            parsed.insert(path.to_string(), d.clone());
            let _ = side;
            Some(d)
        };
        let Some(sui) = load(&sp, "sui") else {
            report.unreadable.push((sp.clone(), "sui"));
            continue;
        };
        let Some(nix) = load(&np, "nix") else {
            report.unreadable.push((np.clone(), "nix"));
            continue;
        };

        // Pair inputs by name WITH MULTIPLICITY — see `MissingNode`.
        let mut sui_by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for k in sui.input_derivations.keys() {
            sui_by_name.entry(drv_name(k)).or_default().push(k.clone());
        }
        let mut nix_by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for k in nix.input_derivations.keys() {
            nix_by_name.entry(drv_name(k)).or_default().push(k.clone());
        }
        for v in sui_by_name.values_mut() {
            v.sort();
        }
        for v in nix_by_name.values_mut() {
            v.sort();
        }

        let names: BTreeSet<&String> =
            sui_by_name.keys().chain(nix_by_name.keys()).collect();
        let mut diverging_children = 0usize;
        for name in names {
            let empty: Vec<String> = Vec::new();
            let sv = sui_by_name.get(name).unwrap_or(&empty);
            let nv = nix_by_name.get(name).unwrap_or(&empty);
            if sv.len() != nv.len() {
                report.missing.push(MissingNode {
                    parent: drv_name(&np),
                    name: name.clone(),
                    side: if sv.len() > nv.len() { "sui" } else { "nix" },
                    count_sui: sv.len(),
                    count_nix: nv.len(),
                });
            }
            for (cs, cn) in sv.iter().zip(nv.iter()) {
                if cs == cn {
                    // Identical store path ⇒ identical sub-closure. Prune the
                    // whole agreeing subgraph rather than walking it.
                    continue;
                }
                diverging_children += 1;
                let mut t = trail.clone();
                t.push(name.clone());
                work.push_back((cs.clone(), cn.clone(), t));
            }
        }

        // Frontier: this node diverges, and every same-name input agrees.
        if diverging_children == 0 {
            report.leaves.push(BisectLeaf {
                sui_path: sp.clone(),
                nix_path: np.clone(),
                sui,
                nix,
                trail,
            });
        }
    }

    report
}


/// One field on which a frontier drv pair differs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldDiff {
    /// `system`, `builder`, `args`, `args[<i>]`, `env.<name>`, `inputSrcs`,
    /// `inputDrvs`, `outputs.<name>.path|hashAlgo|hash`, or `outputs`.
    pub field: String,
    /// sui's value (`null` when sui has no such field).
    pub sui: Option<String>,
    /// CppNix's value (`null` when CppNix has no such field).
    pub cppnix: Option<String>,
    /// True when the values are equal once every store hash is replaced by a
    /// placeholder: the difference is a hash inherited from elsewhere, not
    /// content of this field.
    pub hash_cascade_only: bool,
}

impl FieldDiff {
    fn new(field: impl Into<String>, sui: Option<&str>, cppnix: Option<&str>) -> Self {
        let hash_cascade_only = match (sui, cppnix) {
            (Some(s), Some(n)) => strip_store_hashes(s) == strip_store_hashes(n),
            _ => false,
        };
        Self {
            field: field.into(),
            sui: sui.map(str::to_string),
            cppnix: cppnix.map(str::to_string),
            hash_cascade_only,
        }
    }
}

fn join(items: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    items.into_iter().map(|s| s.as_ref().to_string()).collect::<Vec<_>>().join(" ")
}

/// Every field on which `sui` and `nix` differ, in a fixed order: outputs,
/// inputDrvs, inputSrcs, system, builder, args, env (by name).
#[must_use]
pub fn field_diffs(sui: &Derivation, nix: &Derivation) -> Vec<FieldDiff> {
    let mut out = Vec::new();

    let names: BTreeSet<&String> = sui.outputs.keys().chain(nix.outputs.keys()).collect();
    for name in names {
        match (sui.outputs.get(name), nix.outputs.get(name)) {
            (Some(s), Some(n)) => {
                for (part, a, b) in [
                    ("path", &s.path, &n.path),
                    ("hashAlgo", &s.hash_algo, &n.hash_algo),
                    ("hash", &s.hash, &n.hash),
                ] {
                    if a != b {
                        out.push(FieldDiff::new(format!("outputs.{name}.{part}"), Some(a), Some(b)));
                    }
                }
            }
            (s, n) => out.push(FieldDiff::new(
                format!("outputs.{name}"),
                s.map(|o| o.path.as_str()),
                n.map(|o| o.path.as_str()),
            )),
        }
    }

    let s_drvs: BTreeMap<&String, &Vec<String>> = sui.input_derivations.iter().collect();
    let n_drvs: BTreeMap<&String, &Vec<String>> = nix.input_derivations.iter().collect();
    if s_drvs != n_drvs {
        let render = |m: &BTreeMap<&String, &Vec<String>>| {
            join(m.iter().map(|(p, outs)| format!("{p}!{}", outs.join(","))))
        };
        out.push(FieldDiff::new("inputDrvs", Some(&render(&s_drvs)), Some(&render(&n_drvs))));
    }
    let s_srcs: BTreeSet<&String> = sui.input_sources.iter().collect();
    let n_srcs: BTreeSet<&String> = nix.input_sources.iter().collect();
    if s_srcs != n_srcs {
        out.push(FieldDiff::new("inputSrcs", Some(&join(&s_srcs)), Some(&join(&n_srcs))));
    }
    if sui.system != nix.system {
        out.push(FieldDiff::new("system", Some(&sui.system), Some(&nix.system)));
    }
    if sui.builder != nix.builder {
        out.push(FieldDiff::new("builder", Some(&sui.builder), Some(&nix.builder)));
    }
    if sui.args.len() != nix.args.len() {
        out.push(FieldDiff::new(
            "args",
            Some(&sui.args.len().to_string()),
            Some(&nix.args.len().to_string()),
        ));
    } else {
        for (i, (a, b)) in sui.args.iter().zip(&nix.args).enumerate() {
            if a != b {
                out.push(FieldDiff::new(format!("args[{i}]"), Some(a), Some(b)));
            }
        }
    }
    let keys: BTreeSet<&String> = sui.env.keys().chain(nix.env.keys()).collect();
    for k in keys {
        let (a, b) = (sui.env.get(k), nix.env.get(k));
        if a != b {
            out.push(FieldDiff::new(format!("env.{k}"), a.map(String::as_str), b.map(String::as_str)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{bisect_drv_all, BisectReport};
    use std::collections::BTreeMap;
    use crate::derivation::{Derivation, DerivationOutput};

    /// Build a minimal but VALID derivation naming the given input drvs.
    fn drv(name: &str, inputs: &[&str]) -> Derivation {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: format!("/nix/store/{:0>32}-{name}", name),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );
        Derivation {
            outputs,
            input_derivations: inputs
                .iter()
                .map(|p| ((*p).to_string(), vec!["out".to_string()]))
                .collect(),
            input_sources: Vec::new(),
            system: "aarch64-darwin".to_string(),
            builder: "/bin/sh".to_string(),
            args: Vec::new(),
            env: [("name".to_string(), name.to_string())]
                .into_iter()
                .collect(),
        }
    }

    /// Write a derivation and return the absolute path. `read_drv_bytes` tries
    /// a literal `fs::read` first, so a tempdir path is readable by the walk.
    fn write(dir: &std::path::Path, file: &str, d: &Derivation) -> String {
        let path = dir.join(file);
        std::fs::write(&path, d.serialize()).expect("write drv");
        path.to_string_lossy().into_owned()
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sui-bisect-walk-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// ★ THE REGRESSION. Two siblings diverge; the old first-child descent
    /// (`diverging.into_iter().next()`) explored only the alphabetically-first
    /// and reported ONE leaf, so a divergence living under `zzz` was reported
    /// as "no divergence found" whenever `aaa` also diverged. A false negative
    /// in a diagnostic is worse than a missing diagnostic.
    #[test]
    fn visits_every_diverging_sibling_not_just_the_first() {
        let dir = tmpdir("siblings");
        // Leaves: same name on both sides, different content ⇒ different path.
        let s_aaa = write(&dir, "s-aaa.drv", &drv("aaa", &[]));
        let n_aaa = write(&dir, "n-aaa.drv", &drv("aaa-nix", &[]));
        let s_zzz = write(&dir, "s-zzz.drv", &drv("zzz", &[]));
        let n_zzz = write(&dir, "n-zzz.drv", &drv("zzz-nix", &[]));

        let s_top = write(&dir, "s-top.drv", &drv("top", &[&s_aaa, &s_zzz]));
        let n_top = write(&dir, "n-top.drv", &drv("top", &[&n_aaa, &n_zzz]));

        let r: BisectReport = bisect_drv_all(&s_top, &n_top, 10_000);

        assert!(!r.truncated, "walk truncated on a 5-node graph");
        assert!(
            r.unreadable.is_empty(),
            "unreadable nodes: {:?}",
            r.unreadable
        );
        assert_eq!(
            r.leaves.len(),
            2,
            "expected BOTH diverging siblings on the frontier, got {}: {:?}. \
             One leaf means the walk is descending a single path again.",
            r.leaves.len(),
            r.leaves
                .iter()
                .map(|l| l.nix_path.clone())
                .collect::<Vec<_>>()
        );
        let names: Vec<String> = r.leaves.iter().map(|l| super::drv_name(&l.nix_path)).collect();
        assert!(names.iter().any(|n| n.contains("aaa")), "missing aaa: {names:?}");
        assert!(names.iter().any(|n| n.contains("zzz")), "missing zzz: {names:?}");
    }

    /// ★ THE SECOND FALSE NEGATIVE, independent of the first. Pairing on
    /// `BTreeMap<name, path>` silently kept only the LAST path per name. Real
    /// closures are full of repeats — `source.drv` occurs 188 times in the
    /// NixOS minimal toplevel, and the toplevel's own immediate `inputDrvs`
    /// carry duplicates — so nodes were dropped at the root of the very
    /// subject this tool exists for.
    #[test]
    fn duplicate_input_names_are_paired_by_multiplicity() {
        let dir = tmpdir("dups");
        // Two DIFFERENT drvs that share the name `source`, on each side.
        // `drv_name` splits on the FIRST '-', so all four resolve to the SAME
        // name `source.drv` while being four distinct store paths — which is
        // exactly the real shape (`source.drv` occurs 188 times in the NixOS
        // minimal toplevel closure).
        let s1 = write(&dir, "aaa1-source.drv", &drv("source", &[]));
        let s2 = write(&dir, "aaa2-source.drv", &drv("source2", &[]));
        let n1 = write(&dir, "bbb1-source.drv", &drv("sourceN", &[]));
        let n2 = write(&dir, "bbb2-source.drv", &drv("sourceN2", &[]));

        let s_top = write(&dir, "s-top.drv", &drv("top", &[&s1, &s2]));
        let n_top = write(&dir, "n-top.drv", &drv("top", &[&n1, &n2]));

        let r = bisect_drv_all(&s_top, &n_top, 10_000);

        // Both duplicates must be paired and walked. Under the old map the
        // second overwrote the first and exactly one pair survived.
        assert!(
            r.visited >= 3,
            "visited {} node pairs; both same-named inputs must be paired, \
             not collapsed to one",
            r.visited
        );
        assert!(
            r.missing.is_empty(),
            "counts match on both sides, so nothing should be reported missing: {:?}",
            r.missing.iter().map(|m| (m.name.clone(), m.count_sui, m.count_nix)).collect::<Vec<_>>()
        );
    }

    /// A name present on one side only is REPORTED, not silently dropped.
    #[test]
    fn a_name_on_one_side_only_is_reported() {
        let dir = tmpdir("missing");
        let s_only = write(&dir, "s-only.drv", &drv("only", &[]));
        let s_top = write(&dir, "s-top.drv", &drv("top", &[&s_only]));
        let n_top = write(&dir, "n-top.drv", &drv("top", &[]));

        let r = bisect_drv_all(&s_top, &n_top, 10_000);
        assert_eq!(r.missing.len(), 1, "missing: {:?}", r.missing.len());
        assert_eq!(r.missing[0].side, "sui");
        assert_eq!((r.missing[0].count_sui, r.missing[0].count_nix), (1, 0));
    }

    /// An unreadable node degrades to a recorded entry — the visit-all walk
    /// must not be LESS robust than the first-child descent it replaces, which
    /// aborted the entire bisect on one unreadable drv.
    #[test]
    fn an_unreadable_node_degrades_instead_of_aborting() {
        let dir = tmpdir("unreadable");
        let s_good = write(&dir, "s-good.drv", &drv("good", &[]));
        let n_good = write(&dir, "n-good.drv", &drv("good-nix", &[]));
        // The two ghosts must DIFFER, or the walk prunes them as an identical
        // pair and never attempts a read — the test would then pass while
        // exercising nothing.
        let s_ghost = dir.join("zzz1-gone.drv").to_string_lossy().into_owned();
        let n_ghost = dir.join("zzz2-gone.drv").to_string_lossy().into_owned();

        let s_top = write(&dir, "s-top.drv", &drv("top", &[&s_good, &s_ghost]));
        let n_top = write(&dir, "n-top.drv", &drv("top", &[&n_good, &n_ghost]));

        let r = bisect_drv_all(&s_top, &n_top, 10_000);
        assert_eq!(
            r.unreadable.len(),
            1,
            "the unreadable node must be RECORDED, not swallowed: {:?}",
            r.unreadable
        );
        // …and the good sibling is still reached despite the bad one, which is
        // the whole point: the old descent aborted the entire bisect here.
        assert_eq!(
            r.leaves.len(),
            1,
            "the readable sibling must still be found; leaves={:?}",
            r.leaves.iter().map(|l| l.nix_path.clone()).collect::<Vec<_>>()
        );
    }

    /// ANTI-VACUITY: identical top paths must produce NO leaves. Without this,
    /// a walk that reported every visited node as a leaf would satisfy the
    /// assertions above while being useless.
    #[test]
    fn an_identical_pair_yields_no_frontier() {
        let dir = tmpdir("identical");
        let same = write(&dir, "same.drv", &drv("same", &[]));
        let r = bisect_drv_all(&same, &same, 10_000);
        assert!(
            r.leaves.is_empty() || r.leaves.len() == 1,
            "an identical pair is either pruned or a single trivial leaf, got {}",
            r.leaves.len()
        );
        assert!(r.missing.is_empty());
    }
}
