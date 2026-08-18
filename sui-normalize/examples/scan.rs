//! Blast-radius scanner: run the normalizer over a tree of `.nix` files and
//! report what it would do, without evaluating anything.
//!
//! Two numbers decide whether the normalizer is safe to turn on:
//!
//! * **planned** — files containing at least one binding group with a
//!   duplicate static key or a dotted path. These are the only files whose
//!   answers can change, so this IS the blast radius.
//! * **rejected** — files the normalizer would refuse. Every one of these is
//!   a file nix itself rejects, so a non-zero count is either a real
//!   duplicate in the wild or a bug in this pass. It must be ZERO before the
//!   rejection tier (stage 4) can flip, and each one inspected by hand.
//!
//! Usage: `cargo run -p sui-normalize --example scan -- <dir> [<dir>…]`
//!
//! Deliberately does NOT shell out to nix: this is a pure rnix walk, so it
//! runs over tens of thousands of files in seconds and can be pointed at
//! nixpkgs without a store or an evaluator.

use std::path::{Path, PathBuf};

fn nix_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // ★ `file_type()` does NOT follow symlinks; `Path::is_dir()` DOES.
        // Using `is_dir()` here walked every `result -> /nix/store/...` link
        // in the fleet and pulled the entire store into the scan. Same trap as
        // `DirEntry::metadata()` (lstat) vs `Path::metadata()` (stat).
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            // Skip build output and VCS metadata — neither is source.
            if matches!(name, ".git" | "target" | "result" | "node_modules") {
                continue;
            }
            nix_files(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("nix") {
            out.push(p);
        }
    }
}

fn main() {
    let roots: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if roots.is_empty() {
        eprintln!("usage: scan <dir> [<dir>…]");
        std::process::exit(2);
    }

    let mut files = Vec::new();
    for r in &roots {
        nix_files(r, &mut files);
    }

    let (mut unparseable, mut clean, mut planned, mut rejected) = (0usize, 0usize, 0usize, 0usize);
    let mut groups = 0usize;
    let mut rejects: Vec<(PathBuf, String)> = Vec::new();

    for f in &files {
        let Ok(src) = std::fs::read_to_string(f) else {
            continue;
        };
        let parse = rnix::Root::parse(&src);
        if !parse.errors().is_empty() {
            // rnix could not parse it — not this pass's business, and counted
            // separately so it can never be mistaken for a clean result.
            unparseable += 1;
            continue;
        }
        match sui_normalize::normalize(&parse.tree()) {
            Ok(table) if table.is_empty() => clean += 1,
            Ok(table) => {
                planned += 1;
                groups += table.len();
            }
            Err(e) => {
                rejected += 1;
                rejects.push((f.clone(), e.to_string()));
            }
        }
    }

    println!("scanned    {}", files.len());
    println!("  clean      {clean}   (no duplicate key, no dotted path — untouched)");
    println!("  planned    {planned}   ({groups} binding groups — THE BLAST RADIUS)");
    println!("  rejected   {rejected}   (must be 0 before the rejection tier flips)");
    println!("  unparseable {unparseable}   (rnix could not read; not this pass's business)");

    for (p, e) in rejects.iter().take(25) {
        println!("  REJECT {}: {e}", p.display());
    }
    if rejects.len() > 25 {
        println!("  … and {} more", rejects.len() - 25);
    }
}
