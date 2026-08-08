//! For each STILL-DIVERGING sample file, which structural features does it carry?
//! A file carrying feature F cannot reach parity until F's rule is right, so
//! the count is a CEILING on that rule's remaining parity gain (never a gain).
use std::io::Write;
use std::process::{Command, Stdio};
use rnix::SyntaxKind::*;
/// Oracle from $NIXFMT — never a hardcoded path. A pinned store path was
/// garbage-collected mid-session, after which every call exited 127 and, with
/// stderr discarded at the call site, a total no-op read as success.
fn oracle_bin() -> String {
    std::env::var("NIXFMT").unwrap_or_else(|_| "nixfmt".to_string())
}
fn oracle(src: &str) -> Option<String> {
    let mut c = Command::new(oracle_bin()).arg("--strict").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    c.stdin.as_mut()?.write_all(src.as_bytes()).ok()?;
    let o = c.wait_with_output().ok()?;
    if !o.status.success() { return None }
    String::from_utf8(o.stdout).ok()
}
fn corpus(root: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(e) = std::fs::read_dir(root) else { return };
    for x in e.flatten() { let p = x.path(); let n = x.file_name(); let n = n.to_string_lossy();
        if p.is_dir() { if !matches!(n.as_ref(),"target"|".git"|".claude"|"vendor") && !n.starts_with("result") { corpus(&p,out) } }
        else if p.extension().and_then(|s|s.to_str())==Some("nix") { out.push(p) } }
}
fn main() {
    let mut files = Vec::new();
    for r in ["/Users/drzzln/code/github/pleme-io/nix","/Users/drzzln/code/github/pleme-io/substrate","/Users/drzzln/code/github/pleme-io/sui"] { corpus(std::path::Path::new(r), &mut files) }
    files.sort();
    let step=(files.len()/250).max(1);
    let mut feat: std::collections::BTreeMap<&str,usize> = Default::default();
    let (mut div, mut par) = (0,0);
    for f in files.iter().step_by(step) {
        let Ok(src)=std::fs::read_to_string(f) else { continue };
        let Some(t)=oracle(&src) else { continue };
        let Ok(m)=sui_fmt::format_source(&src) else { continue };
        if m==t { par+=1; continue }
        div+=1;
        let root = rnix::Root::parse(&src).syntax();
        let mut has = |k:&'static str, b:bool| { if b { *feat.entry(k).or_default()+=1 } };
        // trailing comment: comment token whose preceding whitespace has no newline and has a prior sibling
        let mut trailing=false; let mut blockc=false; let mut multiline_str=false; let mut interp=false;
        let mut binop=false; let mut ifelse=false; let mut inherit_multi=false; let mut with_=false;
        for n in root.descendants() {
            match n.kind() {
                NODE_BIN_OP => binop=true,
                NODE_IF_ELSE => ifelse=true,
                NODE_WITH => with_=true,
                NODE_INTERPOL => interp=true,
                NODE_STRING => { let s=n.text().to_string(); if s.starts_with("''") && s.contains('\n') { multiline_str=true } }
                NODE_INHERIT => { if n.children().count()>1 { inherit_multi=true } }
                _=>{}
            }
            let kids: Vec<_> = n.children_with_tokens().collect();
            for (i,c) in kids.iter().enumerate() {
                if let Some(tk)=c.as_token() {
                    if tk.kind()==TOKEN_COMMENT {
                        if tk.text().starts_with("/*") { blockc=true }
                        let mut j=i; let mut nl=false; let mut prior=false;
                        while j>0 { j-=1;
                            match &kids[j] {
                                rowan::NodeOrToken::Token(w) if w.kind()==TOKEN_WHITESPACE => { if w.text().contains('\n') { nl=true } }
                                _ => { prior=true; break }
                            } }
                        if prior && !nl && !tk.text().contains('\n') { trailing=true }
                    }
                }
            }
        }
        has("trailing-comment", trailing);
        has("block-comment /*", blockc);
        has("multi-line '' string", multiline_str);
        has("interpolation ${}", interp);
        has("binary operator", binop);
        has("if/else", ifelse);
        has("inherit with >1 name", inherit_multi);
        has("with/assert", with_);
    }
    println!("diverging {div}  parity {par}");
    let mut v: Vec<_> = feat.into_iter().collect(); v.sort_by_key(|(_,c)| std::cmp::Reverse(*c));
    for (k,c) in v { println!("  {c:>4}  ({:>4.1}% of diverging)  {k}", 100.0*c as f64/div as f64); }
}
