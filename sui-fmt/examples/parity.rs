//! Differential parity against the ORACLE.
//!
//! Adopting RFC 166 wholesale means the canonical form is defined by
//! `nixfmt --strict`, not by us. That converts every style question from an
//! unanswerable matter of taste into a mechanically checkable diff — the same
//! move sui already makes against CppNix for derivation bytes.
//!
//! This reports the parity rate and, more usefully, the DOMINANT DIVERGENCE
//! CLASSES, so the work is driven by frequency rather than by whichever file
//! someone opened.

use std::io::Write;
use std::process::{Command, Stdio};

/// The oracle binary. NOT a hardcoded store path: the pinned one was
/// garbage-collected mid-session, after which every invocation exited 127 —
/// and because stderr was being discarded at the call site, a total no-op
/// reported as "0 files changed". Resolve it from the environment and fail
/// loudly when it is absent.
///
///   NIXFMT=$(nix build --no-link --print-out-paths nixpkgs#nixfmt-rfc-style)/bin/nixfmt
fn oracle_bin() -> String {
    std::env::var("NIXFMT").unwrap_or_else(|_| "nixfmt".to_string())
}

fn oracle(src: &str) -> Option<String> {
    let mut c = Command::new(oracle_bin())
        .arg("--strict")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    c.stdin.as_mut()?.write_all(src.as_bytes()).ok()?;
    let out = c.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

fn corpus(root: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(e) = std::fs::read_dir(root) else { return };
    for x in e.flatten() {
        let p = x.path();
        let n = x.file_name();
        let n = n.to_string_lossy();
        if p.is_dir() {
            if !matches!(n.as_ref(), "target" | ".git" | ".claude" | "vendor")
                && !n.starts_with("result")
            {
                corpus(&p, out);
            }
        } else if p.extension().and_then(|s| s.to_str()) == Some("nix") {
            out.push(p);
        }
    }
}

/// Which of the first differing line's shapes explains the divergence.
fn classify(mine: &str, theirs: &str) -> &'static str {
    let (m, t): (Vec<&str>, Vec<&str>) = (mine.lines().collect(), theirs.lines().collect());
    for i in 0..m.len().max(t.len()) {
        let (a, b) = (m.get(i).copied().unwrap_or(""), t.get(i).copied().unwrap_or(""));
        if a == b {
            continue;
        }
        if a.trim() == b.trim() {
            return "indentation";
        }
        if a.trim_start().starts_with('#') || b.trim_start().starts_with('#') {
            return "comment placement";
        }
        if b.trim().is_empty() || a.trim().is_empty() {
            return "blank line";
        }
        if a.replace(' ', "") == b.replace(' ', "") {
            return "intra-line spacing";
        }
        // One side kept it flat, the other broke it.
        if a.len() > b.len() + 10 || b.len() > a.len() + 10 {
            return "break decision (flat vs broken)";
        }
        return "other";
    }
    "trailing bytes"
}

fn main() {
    let mut files = Vec::new();
    for r in [
        "/Users/drzzln/code/github/pleme-io/nix",
        "/Users/drzzln/code/github/pleme-io/substrate",
        "/Users/drzzln/code/github/pleme-io/sui",
    ] {
        corpus(std::path::Path::new(r), &mut files);
    }
    files.sort();
    // Deterministic sample: every Nth file, so the number is reproducible.
    let step = (files.len() / 250).max(1);
    let sample: Vec<_> = files.iter().step_by(step).collect();

    let (mut parity, mut diverge, mut oracle_fail, mut mine_fail) = (0, 0, 0, 0);
    let mut classes: std::collections::BTreeMap<&str, usize> = Default::default();

    for f in &sample {
        let Ok(src) = std::fs::read_to_string(f) else { continue };
        let Some(theirs) = oracle(&src) else {
            oracle_fail += 1;
            continue;
        };
        let Ok(mine) = sui_fmt::format_source(&src) else {
            mine_fail += 1;
            continue;
        };
        if mine == theirs {
            parity += 1;
        } else {
            diverge += 1;
            *classes.entry(classify(&mine, &theirs)).or_default() += 1;
        }
    }

    let n = parity + diverge;
    println!("sample            {} files (every {}th of {})", sample.len(), step, files.len());
    println!("oracle refused    {oracle_fail}   mine refused {mine_fail}");
    println!(
        "BYTE PARITY       {parity} / {n}  ({:.1}%)",
        100.0 * parity as f64 / n.max(1) as f64
    );
    println!("\ndominant divergence classes (drive the work by these):");
    let mut v: Vec<_> = classes.into_iter().collect();
    v.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (k, c) in v {
        println!("  {c:>4}  {k}");
    }
}
