//! Audit the ORACLE before adopting it wholesale.
//!
//! Adopting RFC 166 means `nixfmt --strict` becomes the definition of
//! correct, and a bootstrap rewrites ~1200 fleet files with it in one commit.
//! Before that, the honest question is whether the tool ever changes MEANING
//! or drops a COMMENT — because a bootstrap that does either is silent,
//! irreversible damage across the whole fleet.
//!
//! This runs our own token-stream + comment law against nixfmt's output. It
//! is the same law the formatter is held to; there is no reason the oracle
//! should be exempt from it.

use std::io::Write;
use std::process::{Command, Stdio};

/// Oracle binary from $NIXFMT — never a hardcoded store path (the pinned
/// one was GC'd mid-session and every call silently exited 127).
fn oracle_bin() -> String {
    std::env::var("NIXFMT").unwrap_or_else(|_| "nixfmt".to_string())
}

fn run(src: &str, strict: bool) -> Option<String> {
    let mut cmd = Command::new(oracle_bin());
    if strict {
        cmd.arg("--strict");
    }
    let mut c = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    c.stdin.as_mut()?.write_all(src.as_bytes()).ok()?;
    let out = c.wait_with_output().ok()?;
    out.status.success().then(|| String::from_utf8(out.stdout).ok())?
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

    let (mut ok, mut meaning, mut comments, mut refused, mut nonidem, mut changed) =
        (0, 0, 0, 0, 0, 0);
    let mut examples: Vec<String> = Vec::new();

    for f in &files {
        let Ok(src) = std::fs::read_to_string(f) else { continue };
        let Some(out) = run(&src, true) else {
            refused += 1;
            continue;
        };
        if out != src {
            changed += 1;
        }
        // Idempotence of the oracle itself.
        if let Some(twice) = run(&out, true) {
            if twice != out {
                nonidem += 1;
                if examples.len() < 10 {
                    examples.push(format!("NON-IDEMPOTENT  {}", f.display()));
                }
            }
        }
        match sui_fmt::law::preserves(&src, &out) {
            Ok(()) => ok += 1,
            Err(sui_fmt::law::LawBreach::CommentLoss { before, after, .. }) => {
                comments += 1;
                if examples.len() < 10 {
                    examples.push(format!("COMMENT LOSS {before}->{after}  {}", f.display()));
                }
            }
            Err(e) => {
                meaning += 1;
                if examples.len() < 10 {
                    examples.push(format!("MEANING CHANGED  {}  ({e})", f.display()));
                }
            }
        }
    }

    println!("=== AUDIT OF nixfmt 1.3.1 --strict OVER THE FLEET ===");
    println!("files                 {}", files.len());
    println!("refused by nixfmt     {refused}");
    println!("WOULD BE REWRITTEN    {changed}   <-- the bootstrap diff");
    println!("law: meaning+comments {ok} ok");
    println!("  meaning CHANGED     {meaning}");
    println!("  comments LOST       {comments}");
    println!("oracle non-idempotent {nonidem}");
    if !examples.is_empty() {
        println!("\n--- findings ---");
        for e in &examples {
            println!("  {e}");
        }
    }
}
