//! Run the formatting laws over the REAL fleet corpus.
//!
//! A law suite is only as strong as what it runs over. blue's own
//! `laws.rs` says so in as many words, and its corpus is 60 hand-written
//! snippets; this drives the same laws over every `.nix` file in the fleet,
//! which is the only way to find out whether the design survives contact.

use std::path::{Path, PathBuf};

fn corpus(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if p.is_dir() {
            // `.claude/worktrees/` holds per-session agent worktrees — copies
            // of this very repo. Counting them would inflate every figure and
            // report the same defect N times.
            if name != "target"
                && name != ".git"
                && name != ".claude"
                && name != "result"
                && name != "vendor"
                && !name.starts_with("result-")
            {
                corpus(&p, out);
            }
        } else if p.extension().and_then(|x| x.to_str()) == Some("nix") {
            out.push(p);
        }
    }
}

fn main() {
    let roots: Vec<PathBuf> = std::env::args()
        .skip(1)
        .map(PathBuf::from)
        .collect();
    let roots = if roots.is_empty() {
        vec![PathBuf::from("/Users/drzzln/code/github/pleme-io/nix")]
    } else {
        roots
    };

    let mut files = Vec::new();
    for r in &roots {
        corpus(r, &mut files);
    }
    files.sort();

    let (mut parsed, mut unparseable) = (0usize, 0usize);
    let (mut law_ok, mut law_broken) = (0usize, 0usize);
    let (mut idem_ok, mut idem_broken) = (0usize, 0usize);
    let mut already_canonical = 0usize;
    let mut panics = 0usize;
    let mut unhandled: Vec<(String, Vec<rnix::SyntaxKind>)> = Vec::new();
    let mut law_examples: Vec<String> = Vec::new();
    let mut idem_examples: Vec<String> = Vec::new();

    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let short = path
            .strip_prefix("/Users/drzzln/code/github/pleme-io/")
            .unwrap_or(path)
            .display()
            .to_string();

        // TOTALITY — a formatter reached by format-on-save must never abort.
        let result = std::panic::catch_unwind(|| sui_fmt::format_source(&src));
        let Ok(result) = result else {
            panics += 1;
            eprintln!("PANIC: {short}");
            continue;
        };

        let Ok(once) = result else {
            unparseable += 1;
            continue;
        };
        parsed += 1;

        if once == src {
            already_canonical += 1;
        }

        let ks = sui_fmt::unhandled_kinds(&src);
        if !ks.is_empty() {
            unhandled.push((short.clone(), ks));
        }

        // LAW: meaning + comments preserved.
        match sui_fmt::law::preserves(&src, &once) {
            Ok(()) => law_ok += 1,
            Err(e) => {
                law_broken += 1;
                if law_examples.len() < 8 {
                    law_examples.push(format!("{short}: {e}"));
                }
            }
        }

        // LAW: idempotence.
        match sui_fmt::format_source(&once) {
            Ok(twice) => {
                if twice == once {
                    idem_ok += 1;
                } else {
                    idem_broken += 1;
                    if idem_examples.len() < 8 {
                        idem_examples.push(short.clone());
                    }
                }
            }
            Err(e) => {
                idem_broken += 1;
                if idem_examples.len() < 8 {
                    idem_examples.push(format!("{short}: output does not re-parse: {e}"));
                }
            }
        }
    }

    println!("corpus            {} .nix files", files.len());
    println!("parsed            {parsed}   unparseable {unparseable}   panics {panics}");
    println!(
        "already canonical {already_canonical} / {parsed}  ({:.1}%)",
        100.0 * already_canonical as f64 / parsed.max(1) as f64
    );
    println!(
        "LAW preserves     {law_ok} ok / {law_broken} BROKEN   ({:.2}% ok)",
        100.0 * law_ok as f64 / parsed.max(1) as f64
    );
    println!(
        "LAW idempotent    {idem_ok} ok / {idem_broken} BROKEN   ({:.2}% ok)",
        100.0 * idem_ok as f64 / parsed.max(1) as f64
    );

    let mut kinds: Vec<rnix::SyntaxKind> = unhandled.iter().flat_map(|(_, k)| k.clone()).collect();
    kinds.sort_by_key(|k| *k as u16);
    kinds.dedup();
    println!(
        "unhandled kinds   {:?} (in {} files)",
        kinds,
        unhandled.len()
    );

    if !law_examples.is_empty() {
        println!("\n--- LAW BREACHES (meaning/comments changed) ---");
        for e in &law_examples {
            println!("  {e}");
        }
    }
    if !idem_examples.is_empty() {
        println!("\n--- NON-IDEMPOTENT ---");
        for e in &idem_examples {
            println!("  {e}");
        }
    }
}
