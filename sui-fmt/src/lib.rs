//! A deterministic Nix formatter over rnix's lossless CST.
//!
//! **There is no configuration type, and that is the feature** — the same
//! rule `blue-lang-fmt` states. A width knob would forfeit the text<->tree
//! bijection: two widths are two canonical forms, and "canonical" then means
//! nothing. The fleet has already paid for the alternative — `caixa-fmt`'s
//! library default is 80 (`config.rs:40`, with a comment justifying "Not
//! 100") while the `feira` CLI that formatted all 568 `.tlisp` files passes
//! `.unwrap_or(100)` (`caixa-feira/src/cmd/fmt.rs:34`). Two sources of truth
//! for one number, disagreeing, and `tatara-kanmon`'s build gate now has to
//! tell people NOT to run `feira fmt` because it "would reformat these
//! straight back into a red build".
//!
//! ## Why this can go further than the tatara-lisp side
//!
//! `tatara-kanmon/build.rs` records exactly why the `.tlisp` gate stops at
//! build-rejected: sealing parse-time "requires the canonical renderer to
//! live at or below `tatara-lisp` so the reader itself can refuse — blocked
//! today because `caixa-fmt` AND `caixa-ast` both take a normal dependency
//! on `tatara-lisp`, making any edge back a hard cargo cycle, and because
//! the reader discards trivia".
//!
//! Neither blocker exists here. `rnix` is an external crate, so
//! `sui-eval -> sui-fmt -> rnix` is acyclic; and rnix's CST is lossless
//! (verified: `Root::parse(s).syntax().text() == s` byte-for-byte across
//! comments, `''` strings, interpolation, `inherit`, CRLF and comment-only
//! files). So the reader itself CAN refuse.

pub mod doc;
pub mod law;

use doc::Doc;
use rnix::{SyntaxKind, SyntaxNode, SyntaxToken};
use rowan::NodeOrToken;

/// The one line width. Not configurable — see the module docs.
pub const WIDTH: usize = 100;

/// One indent step.
const INDENT: isize = 2;

/// Format Nix source into its canonical form.
pub fn format_source(src: &str) -> Result<String, FormatError> {
    let parsed = rnix::Root::parse(src);
    let errors = parsed.errors();
    if !errors.is_empty() {
        // A file that is only comments/whitespace parses with an error
        // ("unexpected EOF") but is a perfectly valid thing to have on disk.
        // Refusing it would make the gate reject a legitimate file.
        if is_trivia_only(src) {
            return Ok(canonical_trivia_only(src));
        }
        return Err(FormatError::Parse(
            errors.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; "),
        ));
    }
    let node = parsed.syntax();
    let rendered = doc::pretty(&root(&node), WIDTH);
    Ok(normalize_trailing(&rendered))
}

/// Is `src` already in canonical form?
///
/// Parses, re-renders and compares BYTES — the formatter's own verdict
/// rather than a reimplementation of it. `tatara-kanmon/build.rs` uses the
/// same shape for the same reason: a gate that re-derives the rule is a
/// second rule that can disagree with the first.
pub fn is_canonical(src: &str) -> bool {
    matches!(format_source(src), Ok(out) if out == src)
}

#[derive(Debug)]
pub enum FormatError {
    Parse(String),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::Parse(m) => write!(f, "does not parse: {m}"),
        }
    }
}

fn is_trivia_only(src: &str) -> bool {
    let p = rnix::Root::parse(src);
    !p.syntax()
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|t| !t.kind().is_trivia())
}

fn canonical_trivia_only(src: &str) -> String {
    let mut out = String::new();
    for line in src.lines() {
        let l = line.trim();
        if !l.is_empty() {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

/// Exactly one trailing newline at end of file.
///
/// It does NOT trim per-line trailing space — `doc::newline` already does
/// that for the space this printer emitted, and doing it here would reach
/// inside `''...''` bodies, where trailing space is part of the value.
fn normalize_trailing(s: &str) -> String {
    let mut out = s.trim_end_matches(['\n', ' ']).to_string();
    out.push('\n');
    out
}

// ---------------------------------------------------------------------------
// Trivia-aware child iteration
//
// This is the whole reason a lossless CST is worth the walk: a comment is a
// TOKEN sitting between two children, so it can be placed STRUCTURALLY — next
// to the thing it documents — rather than re-interleaved by byte position the
// way `blue-lang-fmt` must. blue's `FormatError::UnplaceableComments` class
// therefore has no analogue here: there is no comment position this cannot
// represent.
// ---------------------------------------------------------------------------

enum Piece {
    Node(SyntaxNode),
    Token(SyntaxToken),
    Comment(String),
    /// The author left one or more blank lines here.
    Blank,
}

/// Split a node's children into renderable pieces, preserving comments and
/// collapsing runs of blank lines to at most one.
fn pieces(node: &SyntaxNode) -> Vec<Piece> {
    let mut out = Vec::new();
    for child in node.children_with_tokens() {
        match child {
            NodeOrToken::Node(n) => out.push(Piece::Node(n)),
            NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::TOKEN_COMMENT => out.push(Piece::Comment(render_comment(t.text()))),
                SyntaxKind::TOKEN_WHITESPACE => {
                    // Two or more newlines means the author left a blank line.
                    // Preserving it is not cosmetic: blank lines are how a
                    // reader groups a long attrset, and deleting them all is
                    // the single most-hated thing a formatter can do.
                    if t.text().matches('\n').count() >= 2 && !out.is_empty() {
                        if !matches!(out.last(), Some(Piece::Blank)) {
                            out.push(Piece::Blank);
                        }
                    }
                }
                _ => out.push(Piece::Token(t)),
            },
        }
    }
    out
}

/// `#x` -> `# x`; `#!shebang` and `/* ... */` are left exactly alone.
fn render_comment(text: &str) -> String {
    let t = text.trim_end();
    if t.starts_with("/*") {
        return t.to_string();
    }
    if t.starts_with("#!") {
        return t.to_string();
    }
    // MEASURED (nixfmt --strict 1.3.1): the body after `#` is copied
    // VERBATIM. No space is inserted (`#no space` stays `#no space`), interior
    // indentation is kept (`#   b` stays `#   b`), tabs are kept. Only trailing
    // whitespace is stripped, which collapses a whitespace-only `#   ` to `#`.
    t.to_string()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn root(node: &SyntaxNode) -> Doc {
    let mut d = Doc::nil();
    for p in pieces(node) {
        match p {
            Piece::Node(n) => d = d.concat(expr(&n)),
            Piece::Comment(c) => d = d.concat(Doc::text(c)).concat(Doc::hardline()),
            Piece::Blank => d = d.concat(Doc::hardline()),
            Piece::Token(_) => {}
        }
    }
    d
}

fn expr(node: &SyntaxNode) -> Doc {
    use SyntaxKind::*;
    match node.kind() {
        NODE_ATTR_SET | NODE_LEGACY_LET => attr_set(node),
        NODE_LIST => list(node),
        NODE_LET_IN => let_in(node),
        NODE_ATTRPATH_VALUE => attrpath_value(node),
        NODE_INHERIT => inherit(node),
        NODE_PAREN => paren(node),
        NODE_IF_ELSE => if_else(node),
        NODE_WITH | NODE_ASSERT => with_or_assert(node),
        NODE_LAMBDA => lambda(node),
        NODE_APPLY => apply(node),
        NODE_BIN_OP => bin_op(node),
        NODE_UNARY_OP => unary_op(node),
        NODE_HAS_ATTR => spaced(node),
        NODE_SELECT | NODE_ATTRPATH => tight(node),
        NODE_STRING if is_indented_string(node) => indented_string(node),
        NODE_STRING | NODE_INTERPOL | NODE_DYNAMIC => verbatim(node),
        NODE_IDENT | NODE_LITERAL | NODE_PATH_ABS | NODE_PATH_REL | NODE_PATH_HOME
        | NODE_PATH_SEARCH | NODE_CUR_POS => verbatim(node),
        NODE_ROOT => root(node),
        // Deliberately loud rather than silently plausible. `unknown_kinds`
        // in the coverage test asserts this set is EMPTY over the corpus, so
        // a construct nobody wrote a rule for fails the build instead of
        // round-tripping through a fallback that happens to re-parse.
        _ => verbatim(node),
    }
}

/// Emit a node's source text exactly, minus leading/trailing whitespace.
/// Correct for atoms and for anything whose interior is SEMANTIC — an
/// indented string's body above all, where re-indentation changes the value.
fn verbatim(node: &SyntaxNode) -> Doc {
    Doc::text(node.text().to_string())
}

/// Children separated by single spaces, flattening to a group.
fn spaced(node: &SyntaxNode) -> Doc {
    let parts: Vec<Doc> = pieces(node)
        .into_iter()
        .filter_map(|p| match p {
            Piece::Node(n) => Some(expr(&n)),
            Piece::Token(t) => Some(Doc::text(t.text().to_string())),
            Piece::Comment(c) => Some(Doc::text(c).concat(Doc::hardline())),
            Piece::Blank => None,
        })
        .collect();
    Doc::join(parts, Doc::text(" ")).group()
}

/// Children with no separator — `a.b.c`, an attrpath — EXCEPT `or`, which is
/// a keyword and needs its spaces.
///
/// Gluing it produced `cfg.status or "x"` -> `cfg.statusor "x"`: a different,
/// still-parseable identifier. Nothing about the output looks wrong at a
/// glance, which is exactly why the token-stream law rather than an eyeball
/// is what caught it, across 8 files at once.
fn tight(node: &SyntaxNode) -> Doc {
    let mut out = Doc::nil();
    for p in pieces(node) {
        match p {
            Piece::Node(n) => out = out.concat(expr(&n)),
            Piece::Token(t) if t.kind() == SyntaxKind::TOKEN_OR => {
                out = out
                    .concat(Doc::text(" "))
                    .concat(Doc::text(t.text().to_string()))
                    .concat(Doc::text(" "));
            }
            Piece::Token(t) => out = out.concat(Doc::text(t.text().to_string())),
            Piece::Comment(c) => out = out.concat(Doc::text(c)).concat(Doc::hardline()),
            Piece::Blank => {}
        }
    }
    out
}

/// `{ a = 1; b = 2; }` — flat when it fits, one binding per line otherwise.
fn attr_set(node: &SyntaxNode) -> Doc {
    let mut lead = Doc::nil();
    let mut items: Vec<Doc> = Vec::new();
    let mut pending_blank = false;

    for p in pieces(node) {
        match p {
            Piece::Token(t) => match t.kind() {
                SyntaxKind::TOKEN_REC | SyntaxKind::TOKEN_LET => {
                    lead = lead.concat(Doc::text(t.text().to_string())).concat(Doc::text(" "));
                }
                _ => {}
            },
            Piece::Blank => pending_blank = true,
            Piece::Comment(c) => {
                if pending_blank && !items.is_empty() {
                    items.push(Doc::nil());
                    pending_blank = false;
                }
                items.push(Doc::text(c));
            }
            Piece::Node(n) => {
                if pending_blank && !items.is_empty() {
                    items.push(Doc::nil());
                    pending_blank = false;
                }
                items.push(expr(&n));
            }
        }
    }

    if items.is_empty() {
        return lead.concat(Doc::text("{ }"));
    }

    // A comment anywhere inside forces the broken shape: a `#` comment
    // consumes the rest of its line, so flattening `{ # c
    // a = 1; }` onto one line would swallow the binding INTO the comment.
    // That is a semantic change, not a layout one.
    let has_comment = node
        .children_with_tokens()
        .filter_map(|c| c.into_token())
        .any(|t| t.kind() == SyntaxKind::TOKEN_COMMENT);

    let n_items = node
        .children()
        .filter(|c| {
            matches!(
                c.kind(),
                SyntaxKind::NODE_ATTRPATH_VALUE | SyntaxKind::NODE_INHERIT
            )
        })
        .count();
    let wide = attrset_forced_wide(node);
    let sep = if has_comment || wide || n_items >= 2 {
        Doc::hardline()
    } else {
        Doc::line()
    };
    let inner = Doc::join(items, sep.clone());
    lead.concat(
        Doc::text("{")
            .concat(sep.clone().concat(inner).nest(INDENT))
            .concat(sep)
            .concat(Doc::text("}"))
            .group(),
    )
}

/// `[ a b c ]`
fn list(node: &SyntaxNode) -> Doc {
    let mut items: Vec<Doc> = Vec::new();
    let mut has_comment = false;
    for p in pieces(node) {
        match p {
            Piece::Node(n) => items.push(expr(&n)),
            Piece::Comment(c) => {
                has_comment = true;
                items.push(Doc::text(c));
            }
            Piece::Blank | Piece::Token(_) => {}
        }
    }
    if items.is_empty() {
        return Doc::text("[ ]");
    }
    let n_items = node.children().count();
    let sep = if has_comment || n_items >= 2 { Doc::hardline() } else { Doc::line() };
    Doc::text("[")
        .concat(sep.clone().concat(Doc::join(items, sep.clone())).nest(INDENT))
        .concat(sep)
        .concat(Doc::text("]"))
        .group()
}





fn pattern_expands(n: &SyntaxNode) -> bool {
    use SyntaxKind::*;
    if n.kind() != NODE_PATTERN {
        return false;
    }
    let named = n.children().filter(|c| c.kind() == NODE_PAT_ENTRY).count();
    let has_default = n.children().any(|c| {
        c.kind() == NODE_PAT_ENTRY
            && c.children_with_tokens()
                .filter_map(|x| x.into_token())
                .any(|t| t.kind() == TOKEN_QUESTION)
    });
    let has_comment = n
        .children_with_tokens()
        .filter_map(|x| x.into_token())
        .any(|t| t.kind() == TOKEN_COMMENT);
    named >= 3 || has_default || has_comment
}

fn has_at_bind(n: &SyntaxNode) -> bool {
    n.kind() == SyntaxKind::NODE_PATTERN
        && n.children().any(|c| c.kind() == SyntaxKind::NODE_PAT_BIND)
}

/// Can this expression's opening delimiter hug a lambda colon?
fn hug_absorbable(n: &SyntaxNode) -> bool {
    use SyntaxKind::*;
    match n.kind() {
        NODE_ATTR_SET | NODE_LEGACY_LET | NODE_LIST => n.children().next().is_some(),
        NODE_STRING => n.text().to_string().starts_with("''") && n.text().to_string().contains('\n'),
        NODE_WITH => n.children().last().is_some_and(|b| hug_absorbable(&b)),
        NODE_PAREN => n
            .children()
            .next()
            .is_some_and(|c| matches!(c.kind(), NODE_ATTR_SET | NODE_LIST) && c.children().next().is_some()),
        _ => false,
    }
}

/// Does this abstraction chain end in a term its colon can hug?
fn lambda_hugs(n: &SyntaxNode) -> bool {
    if n.kind() != SyntaxKind::NODE_LAMBDA {
        return hug_absorbable(n);
    }
    let Some(param) = n.children().next() else { return false };
    if param.kind() == SyntaxKind::NODE_PATTERN && (has_at_bind(&param) || pattern_expands(&param)) {
        return false;
    }
    match n.children().last() {
        Some(b) => lambda_hugs(&b),
        None => false,
    }
}

/// Number of arguments in the whole left-associated apply spine this node
/// belongs to. `f x y` -> 2, counted from the OUTERMOST apply.
fn apply_chain_args(node: &SyntaxNode) -> usize {
    let mut top = node.clone();
    while let Some(p) = top.parent() {
        if p.kind() == SyntaxKind::NODE_APPLY && p.children().next().map(|c| c == top).unwrap_or(false) {
            top = p;
        } else {
            break;
        }
    }
    let mut n = 0usize;
    let mut cur = top;
    while cur.kind() == SyntaxKind::NODE_APPLY {
        n += 1;
        match cur.children().next() {
            Some(f) => cur = f,
            None => break,
        }
    }
    n
}

/// Positions that force an attrset one-per-line regardless of item count or
/// width. Lists have no such rule.
fn attrset_forced_wide(node: &SyntaxNode) -> bool {
    use SyntaxKind::*;
    let Some(parent) = node.parent() else { return false };
    let is_last = parent.children().last().map(|c| c == *node).unwrap_or(false);
    match parent.kind() {
        // (a) direct RHS of a binding, AND it holds at least one `k = v;`
        NODE_ATTRPATH_VALUE if is_last => node
            .children()
            .any(|c| c.kind() == NODE_ATTRPATH_VALUE),
        // (b)/(c) body of a `let ... in` / `assert c;` -- predicate is just
        // "non-empty", an inherit-only set expands here too.
        NODE_LET_IN | NODE_ASSERT if is_last => node.children().next().is_some(),
        _ => false,
    }
}


/// A multi-line `''...''` string, RE-INDENTED.
///
/// nixfmt does re-indent these: the common leading indentation is stripped
/// and the body re-emitted at (opening line indent + 2). That is value-
/// preserving because Nix strips the common indent itself at parse time.
fn indented_string(node: &SyntaxNode) -> Doc {
    enum Seg {
        Txt(String),
        Interp(SyntaxNode),
    }
    let mut segs: Vec<Seg> = Vec::new();
    for c in node.children_with_tokens() {
        match c {
            NodeOrToken::Token(t) => {
                if t.kind() == SyntaxKind::TOKEN_STRING_CONTENT {
                    segs.push(Seg::Txt(t.text().to_string()));
                }
            }
            NodeOrToken::Node(n) => segs.push(Seg::Interp(n)),
        }
    }
    // Split into lines of pieces.
    let mut lines: Vec<Vec<Seg>> = vec![Vec::new()];
    for s in segs {
        match s {
            Seg::Interp(n) => lines.last_mut().unwrap().push(Seg::Interp(n)),
            Seg::Txt(t) => {
                let mut first = true;
                for part in t.split('\n') {
                    if !first {
                        lines.push(Vec::new());
                    }
                    first = false;
                    if !part.is_empty() {
                        lines.last_mut().unwrap().push(Seg::Txt(part.to_string()));
                    }
                }
            }
        }
    }
    // The opening line and the closer's line drop out when blank.
    let blank = |l: &Vec<Seg>| {
        l.iter().all(|s| match s {
            Seg::Txt(t) => t.chars().all(|c| c == ' ' || c == '\t'),
            Seg::Interp(_) => false,
        })
    };
    if !lines.is_empty() && blank(&lines[0]) {
        lines.remove(0);
    }
    // Whether the CLOSER sat on its own line, which is part of the VALUE:
    //   ''\n  a\n''  -> "a\n"      (closer on its own line)
    //   ''\n  a''    -> "a"        (closer on the content line)
    // Emitting the trailing newline unconditionally added a `\n` to every
    // string of the second shape — a real value change, caught by the law on
    // 8 files (substrate's lockfile-delta, cargo-nix-tie, format-ban, …), all
    // of them assertion messages whose text is what an operator reads.
    let closer_on_own_line = lines.len() > 1 && blank(lines.last().unwrap());
    if closer_on_own_line {
        lines.pop();
    }
    // Minimum leading-SPACE count over non-blank lines. A tab terminates the
    // run, so a tab-led line measures 0.
    let lead = |l: &Vec<Seg>| -> usize {
        match l.first() {
            Some(Seg::Txt(t)) => t.chars().take_while(|c| *c == ' ').count(),
            _ => 0,
        }
    };
    let m = lines
        .iter()
        .filter(|l| !blank(l))
        .map(lead)
        .min()
        .unwrap_or(0);

    let mut body = Doc::nil();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            body = body.concat(Doc::hardline());
        }
        if blank(l) {
            // A whitespace-only line narrower than the common indent becomes
            // completely empty; a wider one keeps what is left.
            let w: usize = l
                .iter()
                .map(|s| match s {
                    Seg::Txt(t) => t.chars().count(),
                    Seg::Interp(_) => 0,
                })
                .sum();
            if w > m {
                body = body.concat(Doc::text(" ".repeat(w - m)));
            }
            continue;
        }
        let mut first = true;
        for s in l {
            match s {
                Seg::Txt(t) => {
                    let t = if first {
                        let k = t.chars().take_while(|c| *c == ' ').count().min(m);
                        t.chars().skip(k).collect::<String>()
                    } else {
                        t.clone()
                    };
                    body = body.concat(Doc::text(t));
                }
                Seg::Interp(n) => body = body.concat(verbatim(n)),
            }
            first = false;
        }
    }
    let opened = Doc::text("''").concat(Doc::hardline().concat(body).nest(INDENT));
    if closer_on_own_line {
        opened.concat(Doc::hardline()).concat(Doc::text("''"))
    } else {
        // No break before the closer: the value ends without a newline and
        // must keep ending without one.
        opened.concat(Doc::text("''"))
    }
}

/// Only reflow the SAFE shape: an `''` string whose opening line is blank.
/// A single-line `''x''` reflowed to three lines gains a trailing newline in
/// its VALUE, and text left on the opening line makes the common-indent
/// computation disagree with Nix's own. Both were caught by the law, not by
/// eye -- 153 files at once.
fn is_indented_string(node: &SyntaxNode) -> bool {
    if node.kind() != SyntaxKind::NODE_STRING {
        return false;
    }
    let t = node.text().to_string();
    let Some(rest) = t.strip_prefix("''") else { return false };
    match rest.find('\n') {
        Some(i) => rest[..i].chars().all(|c| c == ' ' || c == '\t'),
        None => false,
    }
}

/// A term whose opening delimiter can be hugged onto the current line: the
/// enclosing group is judged on that first line alone.
fn is_absorbable(node: &SyntaxNode) -> bool {
    use SyntaxKind::*;
    match node.kind() {
        NODE_ATTR_SET | NODE_LEGACY_LET => node.children().next().is_some(),
        NODE_LIST => node.children().next().is_some(),
        NODE_PAREN => true,
        NODE_STRING => node.text().to_string().starts_with("''"),
        _ => false,
    }
}

/// `a.b = expr;`
fn attrpath_value(node: &SyntaxNode) -> Doc {
    let mut path = Doc::nil();
    let mut value = Doc::nil();
    let mut value_node: Option<SyntaxNode> = None;
    let mut seen_assign = false;
    let mut lead = Doc::nil();
    // Comments between `=` and the value are LEADING TRIVIA OF THE VALUE, not
    // of the binding. Hoisting them above `path =` is a RELOCATION — nixfmt
    // keeps them where they were, which forces the `path =` / newline / value
    // shape at one extra indent step.
    let mut value_lead = Doc::nil();
    let mut value_lead_n = 0usize;

    for p in pieces(node) {
        match p {
            Piece::Token(t) if t.kind() == SyntaxKind::TOKEN_ASSIGN => seen_assign = true,
            Piece::Blank if seen_assign && value_lead_n > 0 => {
                value_lead = value_lead.concat(Doc::hardline());
            }
            Piece::Comment(c) if seen_assign => {
                value_lead = value_lead.concat(Doc::text(c)).concat(Doc::hardline());
                value_lead_n += 1;
            }
            Piece::Comment(c) => lead = lead.concat(Doc::text(c)).concat(Doc::hardline()),
            Piece::Node(n) => {
                if seen_assign {
                    value = expr(&n);
                    value_node = Some(n);
                } else {
                    path = expr(&n);
                }
            }
            _ => {}
        }
    }
    if value_lead_n > 0 {
        return lead.concat(
            path.concat(Doc::text(" ="))
                .concat(
                    Doc::hardline()
                        .concat(value_lead)
                        .concat(value)
                        .nest(INDENT),
                )
                .concat(Doc::text(";")),
        );
    }
    let value = if value_node.as_ref().is_some_and(is_absorbable) {
        value.absorb()
    } else {
        value
    };
    lead.concat(
        path.concat(Doc::text(" ="))
            .concat(Doc::line().concat(value).nest_if_broken(INDENT))
            .group()
            .concat(Doc::text(";")),
    )
}

/// `inherit a b;` / `inherit (pkgs) hello;`
fn inherit(node: &SyntaxNode) -> Doc {
    let mut d = Doc::text("inherit");
    for p in pieces(node) {
        match p {
            Piece::Node(n) => {
                d = d.concat(Doc::text(" ")).concat(expr(&n));
            }
            Piece::Comment(c) => {
                d = d
                    .concat(Doc::hardline())
                    .concat(Doc::text(c))
                    .concat(Doc::hardline());
            }
            _ => {}
        }
    }
    d.concat(Doc::text(";"))
}

fn paren(node: &SyntaxNode) -> Doc {
    let inner: Vec<Doc> = pieces(node)
        .into_iter()
        .filter_map(|p| match p {
            Piece::Node(n) => Some(expr(&n)),
            Piece::Comment(c) => Some(Doc::text(c).concat(Doc::hardline())),
            _ => None,
        })
        .collect();
    Doc::text("(")
        .concat(Doc::softline().concat(Doc::concat_all(inner)).nest(INDENT))
        .concat(Doc::softline())
        .concat(Doc::text(")"))
        .group()
}

/// `let a = 1; in body` — `let` bodies ALWAYS break. A `let` collapsed onto
/// one line is legal Nix and unreadable at any size above trivial, and
/// allowing both shapes would mean one tree has two canonical forms.
fn let_in(node: &SyntaxNode) -> Doc {
    let mut binds: Vec<Doc> = Vec::new();
    let mut body = Doc::nil();
    let mut body_lead = Doc::nil();
    let mut seen_in = false;
    let mut pending_blank = false;

    for p in pieces(node) {
        match p {
            Piece::Token(t) if t.kind() == SyntaxKind::TOKEN_IN => seen_in = true,
            Piece::Blank => pending_blank = true,
            Piece::Comment(c) => {
                // AFTER `in`, the comment is leading trivia of the BODY, not a
                // last binding. Filing it into `binds` printed it above `in`,
                // which relocates it past a keyword.
                if seen_in {
                    body_lead = body_lead.concat(Doc::text(c)).concat(Doc::hardline());
                    continue;
                }
                if pending_blank && !binds.is_empty() {
                    binds.push(Doc::nil());
                    pending_blank = false;
                }
                binds.push(Doc::text(c));
            }
            Piece::Node(n) => {
                if seen_in {
                    body = expr(&n);
                } else {
                    if pending_blank && !binds.is_empty() {
                        binds.push(Doc::nil());
                        pending_blank = false;
                    }
                    binds.push(expr(&n));
                }
            }
            _ => {}
        }
    }

    Doc::text("let")
        .concat(
            Doc::hardline()
                .concat(Doc::join(binds, Doc::hardline()))
                .nest(INDENT),
        )
        .concat(Doc::hardline())
        .concat(Doc::text("in"))
        .concat(Doc::hardline())
        .concat(body_lead)
        .concat(body)
}

/// `x: body` / `{ a, b ? 1, ... }@args: body`
fn lambda(node: &SyntaxNode) -> Doc {
    let mut param = Doc::nil();
    let mut body = Doc::nil();
    // Comments sitting between `:` and the body are LEADING TRIVIA of the body:
    // they render on their own lines, at the body's indent, immediately above
    // it. Accumulating them into `body` and then assigning `body = expr(&n)`
    // DROPPED them outright (`x:\n# c\nx` rendered as `x: x`).
    let mut body_lead = Doc::nil();
    let mut seen_colon = false;
    for p in pieces(node) {
        match p {
            Piece::Token(t) if t.kind() == SyntaxKind::TOKEN_COLON => seen_colon = true,
            Piece::Node(n) => {
                if seen_colon {
                    body = expr(&n);
                } else {
                    param = pattern_or_ident(&n);
                }
            }
            Piece::Comment(c) => {
                if seen_colon {
                    body_lead = body_lead.concat(Doc::text(c)).concat(Doc::hardline());
                } else {
                    param = param.concat(Doc::text(c)).concat(Doc::hardline());
                }
            }
            _ => {}
        }
    }
    let body = body_lead.concat(body);
    // A lambda body goes on the same line when it fits, and on the next line
    // (NOT indented) when it does not — the standard Nix shape for the
    // `{ pkgs, ... }: { ... }` module idiom, where indenting the body would
    // push every module one step right for no gain.
    if lambda_hugs(node) {
        return param
            .concat(Doc::text(":"))
            .concat(Doc::text(" "))
            .concat(body.absorb());
    }
    param
        .concat(Doc::text(":"))
        .concat(Doc::line())
        .concat(body)
        .group()
}

fn pattern_or_ident(node: &SyntaxNode) -> Doc {
    if node.kind() != SyntaxKind::NODE_PATTERN {
        return expr(node);
    }
    let mut entries: Vec<(Doc, bool)> = Vec::new();
    let mut bind_before = Doc::nil();
    let mut bind_after = Doc::nil();
    let mut seen_brace = false;
    let mut ellipsis = false;

    for child in node.children_with_tokens() {
        match child {
            NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::TOKEN_L_BRACE => seen_brace = true,
                SyntaxKind::TOKEN_ELLIPSIS => ellipsis = true,
                // A comment inside a formal pattern is the single most common
                // place one appears in this fleet — `{ config, # unused\n
                // pkgs, ... }` — and dropping it silently is the exact data
                // loss `blue-lang-fmt` calls "the one part of a program a
                // machine cannot reconstruct". Ignoring TOKEN_COMMENT here
                // lost 5 to 10 comments per file across 5 fleet files.
                SyntaxKind::TOKEN_COMMENT => {
                    entries.push((Doc::text(render_comment(t.text())), true))
                }
                _ => {}
            },
            NodeOrToken::Node(n) => match n.kind() {
                SyntaxKind::NODE_PAT_ENTRY => entries.push((spaced(&n), false)),
                // rnix wraps the `@`-binding in its own NODE_PAT_BIND, on
                // EITHER side of the brace. Letting it fall through to the
                // catch-all rendered `inputs @` as a pattern ENTRY, producing
                // `{ inputs @, self, ... }` — which does not re-parse. Caught
                // by the token-stream law over the fleet corpus, not by any
                // hand-written case.
                SyntaxKind::NODE_PAT_BIND | SyntaxKind::NODE_IDENT => {
                    let name = n
                        .children()
                        .find(|c| c.kind() == SyntaxKind::NODE_IDENT)
                        .map_or_else(|| expr(&n), |i| expr(&i));
                    if seen_brace {
                        bind_after = Doc::text("@").concat(name);
                    } else {
                        bind_before = name.concat(Doc::text("@"));
                    }
                }
                _ => entries.push((expr(&n), false)),
            },
        }
    }
    if ellipsis {
        entries.push((Doc::text("..."), false));
    }
    if entries.is_empty() {
        return bind_before.concat(Doc::text("{ }")).concat(bind_after);
    }

    // A comment forces the broken shape and takes NO comma: `# note,` would
    // put the separator inside the comment, where the parser can never see it.
    let has_comment = entries.iter().any(|(_, c)| *c);
    // MEASURED: a pattern expands at >= 3 named formals, or if ANY formal
    // carries a `? default`. `...` is not a formal and does not count.
    let named = node
        .children()
        .filter(|c| c.kind() == SyntaxKind::NODE_PAT_ENTRY)
        .count();
    let has_default = node.children().any(|c| {
        c.kind() == SyntaxKind::NODE_PAT_ENTRY
            && c.children_with_tokens()
                .filter_map(|x| x.into_token())
                .any(|t| t.kind() == SyntaxKind::TOKEN_QUESTION)
    });
    let hard = has_comment || named >= 3 || has_default;
    let sep = if hard { Doc::hardline() } else { Doc::line() };

    let last_real = entries.iter().rposition(|(_, c)| !*c);
    let mut inner = Doc::nil();
    for (i, (d, is_comment)) in entries.iter().enumerate() {
        if i > 0 {
            inner = inner.concat(sep.clone());
        }
        inner = inner.concat(d.clone());
        // In the BROKEN form every named formal takes a comma, the last one
        // included; `...` never does. In the flat form the last one does not.
        let is_ellipsis = ellipsis && Some(i) == last_real;
        if !*is_comment && !is_ellipsis && (hard || Some(i) != last_real) {
            inner = inner.concat(Doc::text(","));
        }
    }

    bind_before
        .concat(
            Doc::text("{")
                .concat(sep.clone().concat(inner).nest(INDENT))
                .concat(sep)
                .concat(Doc::text("}"))
                .group(),
        )
        .concat(bind_after)
}

/// `f a b` — the head stays put; arguments indent when they break.
fn apply(node: &SyntaxNode) -> Doc {
    let parts: Vec<Doc> = pieces(node)
        .into_iter()
        .filter_map(|p| match p {
            Piece::Node(n) => {
                let d = expr(&n);
                Some(if is_absorbable(&n) { d.absorb() } else { d })
            }
            Piece::Comment(c) => Some(Doc::text(c).concat(Doc::hardline())),
            _ => None,
        })
        .collect();
    if parts.len() < 2 {
        return Doc::concat_all(parts);
    }
    // MEASURED: a chain of <= 2 arguments has NO break point at any width.
    if apply_chain_args(node) <= 2 {
        return Doc::join(parts, Doc::text(" "));
    }
    let mut it = parts.into_iter();
    let head = it.next().unwrap_or_else(Doc::nil);
    let rest = Doc::join(it, Doc::line());
    head.concat(Doc::line().concat(rest).nest_if_broken(INDENT)).group()
}


fn binop_token(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(|c| c.into_token())
        .find(|t| !t.kind().is_trivia())
}

/// Nix precedence class of a binary operator token.
fn prec_of(t: &SyntaxToken) -> u8 {
    match t.text() {
        "++" => 5,
        "*" | "/" => 6,
        "+" | "-" => 7,
        "//" => 9,
        "<" | ">" | "<=" | ">=" => 10,
        "==" | "!=" => 11,
        "&&" => 12,
        "||" => 13,
        "->" => 14,
        _ => 99,
    }
}

/// Flatten a maximal same-precedence operator run.
fn flatten_chain(node: &SyntaxNode, p: u8, ops: &mut Vec<String>, operands: &mut Vec<SyntaxNode>) {
    if node.kind() == SyntaxKind::NODE_BIN_OP {
        if let Some(t) = binop_token(node) {
            if prec_of(&t) == p {
                let kids: Vec<SyntaxNode> = node.children().collect();
                if kids.len() == 2 {
                    flatten_chain(&kids[0], p, ops, operands);
                    ops.push(t.text().to_string());
                    flatten_chain(&kids[1], p, ops, operands);
                    return;
                }
            }
        }
    }
    operands.push(node.clone());
}

fn bin_op(node: &SyntaxNode) -> Doc {
    // DESCENDANTS, not direct children. `flatten_chain` descends past inner
    // same-precedence `NODE_BIN_OP`s, so a comment attached to one of THOSE
    // is consumed by the flatten and never rendered. Checking only this
    // node's own tokens lost 34 comments in one file
    // (modules/pleme/nixos/node-budget.nix, 227 -> 193) and broke the law on
    // 13 files — a comment sitting between two operands of a `++` chain is an
    // extremely common shape in this fleet.
    //
    // Deliberately CONSERVATIVE: any comment anywhere beneath this node sends
    // the whole chain down the safe path. That gives up some parity on chains
    // whose comment sits deep inside an operand and could in principle have
    // been kept, and that trade is correct — parity bought by dropping a
    // comment is not parity, it is data loss with a better score.
    let has_comment = node
        .descendants_with_tokens()
        .filter_map(|c| c.into_token())
        .any(|t| t.kind() == SyntaxKind::TOKEN_COMMENT);
    let tok = binop_token(node);
    let kids: Vec<SyntaxNode> = node.children().collect();
    if has_comment || tok.is_none() || kids.len() != 2 {
        let parts: Vec<Doc> = pieces(node)
            .into_iter()
            .filter_map(|p| match p {
                Piece::Node(n) => Some(expr(&n)),
                Piece::Token(t) => Some(Doc::text(t.text().to_string())),
                Piece::Comment(c) => Some(Doc::text(c).concat(Doc::hardline())),
                Piece::Blank => None,
            })
            .collect();
        return Doc::join(parts, Doc::text(" ")).group();
    }
    let tok = tok.unwrap();
    // Only the OUTERMOST node of a same-precedence run lays the chain out.
    let p = prec_of(&tok);
    if let Some(parent) = node.parent() {
        if parent.kind() == SyntaxKind::NODE_BIN_OP
            && binop_token(&parent).map(|t| prec_of(&t)) == Some(p)
        {
            // handled by the ancestor; render as a plain flat join so the
            // ancestor's flatten sees it -- unreachable in practice because
            // flatten_chain descends past us.
        }
    }
    let mut ops = Vec::new();
    let mut operands = Vec::new();
    flatten_chain(node, p, &mut ops, &mut operands);

    // Non-associative comparisons with an absorbable RHS have NO break point.
    let comparison = matches!(p, 10 | 11);
    if ops.len() == 1 && comparison && is_absorbable(&operands[1]) {
        return expr(&operands[0])
            .concat(Doc::text(" "))
            .concat(Doc::text(ops[0].clone()))
            .concat(Doc::text(" "))
            .concat(expr(&operands[1]).absorb());
    }
    let mut d = expr(&operands[0]);
    for (i, op) in ops.iter().enumerate() {
        let o = &operands[i + 1];
        let item = if is_absorbable(o) {
            Doc::text(op.clone())
                .concat(Doc::text(" "))
                .concat(expr(o).absorb())
        } else {
            Doc::text(op.clone())
                .concat(Doc::line().concat(expr(o)).nest_if_broken(INDENT))
                .group()
        };
        d = d.concat(Doc::line()).concat(item);
    }
    d.group()
}

fn unary_op(node: &SyntaxNode) -> Doc {
    let parts: Vec<Doc> = pieces(node)
        .into_iter()
        .filter_map(|p| match p {
            Piece::Node(n) => Some(expr(&n)),
            Piece::Token(t) => Some(Doc::text(t.text().to_string())),
            Piece::Comment(c) => Some(Doc::text(c).concat(Doc::hardline())),
            Piece::Blank => None,
        })
        .collect();
    Doc::concat_all(parts)
}

/// `if c then a else b`
fn if_else(node: &SyntaxNode) -> Doc {
    let mut arms: Vec<(String, Doc)> = Vec::new();
    let mut kw = String::from("if");
    let mut lead = Doc::nil();
    for p in pieces(node) {
        match p {
            Piece::Token(t) => match t.kind() {
                SyntaxKind::TOKEN_THEN => kw = "then".into(),
                SyntaxKind::TOKEN_ELSE => kw = "else".into(),
                _ => {}
            },
            Piece::Node(n) => arms.push((kw.clone(), expr(&n))),
            // A comment between `if` and its condition has nowhere structural
            // to go, so it leads the whole form. Dropping it was silent data
            // loss — see `with_or_assert`.
            Piece::Comment(c) => lead = lead.concat(Doc::text(c)).concat(Doc::hardline()),
            Piece::Blank => {}
        }
    }

    // Shape derived by EXPERIMENT against `nixfmt --strict`, not from prose:
    //
    //   { a = if p then q else r; }                    -> stays flat
    //   { a = if p then q else if s then t else u; }   -> ALWAYS breaks (44 cols)
    //   single if/else, line length 100                -> flat
    //   single if/else, line length 101                -> broken
    //
    // So there are TWO rules, and only one of them is about width:
    //   * a single if/else is ordinary width-driven `group` behaviour, and the
    //     boundary is exactly the 100-column width (bisected: 100 flat,
    //     101 broken);
    //   * an ELSE-IF CHAIN always breaks no matter how short it is. That is a
    //     COUNT rule, the same shape as the n>=2 attrset rule, not a width one
    //     — which is why treating `if` as purely width-driven never converges.
    //
    // Broken layout, with `then` staying on the `if` line and `else` returning
    // to the `if`'s own indent:
    //
    //   if <cond> then
    //     <then>
    //   else if <cond> then
    //     <then>
    //   else
    //     <else>
    let (cond, then_br, else_br) = (arms.first(), arms.get(1), arms.get(2));
    let Some((_, cond)) = cond else {
        return lead.concat(verbatim(node));
    };
    let Some((_, then_br)) = then_br else {
        return lead.concat(verbatim(node));
    };

    // `else if` — the else arm is itself an if. nixfmt keeps the nested `if` on
    // the SAME line as `else` and at the SAME indent, so the chain reads as one
    // ladder rather than a staircase drifting right.
    let else_is_chain = node
        .children()
        .nth(2)
        .is_some_and(|n| n.kind() == SyntaxKind::NODE_IF_ELSE);

    // …and the chain propagates DOWNWARD. An `if` that is itself the else-arm
    // of another `if` must break too, or the ladder collapses halfway:
    //
    //   if p then          <- outer broke
    //     q
    //   else if s then t else u;    <- inner stayed flat, because ITS own
    //                                  else is a plain value and it fits
    //
    // nixfmt breaks the whole ladder as one unit. Detected from the parent
    // rather than threaded through as a parameter, so the rule holds no matter
    // which entry point rendered this node.
    let is_else_arm = node.parent().is_some_and(|p| {
        p.kind() == SyntaxKind::NODE_IF_ELSE
            && p.children().nth(2).is_some_and(|n| n == *node)
    });

    // TWO DISTINCT FACTS, and conflating them renders `else u;` instead of a
    // broken final arm:
    //   * `else_is_chain` — MY else-arm is an `if`, so the tail is emitted
    //     inline as `else <if>` at this indent;
    //   * `force_break`   — this `if` participates in a ladder at all (either
    //     because it chains, or because it IS someone's else-arm), so every
    //     separator is hard.
    // The inner link of a ladder has force_break=true and else_is_chain=false.
    let force_break = else_is_chain || is_else_arm;

    let sep = if force_break {
        Doc::hardline()
    } else {
        Doc::line()
    };

    let head = Doc::text("if ")
        .concat(cond.clone())
        .concat(Doc::text(" then"))
        .concat(sep.clone().concat(then_br.clone()).nest_if_broken(INDENT));

    let tail = match else_br {
        None => Doc::nil(),
        // A chained `else if` is emitted inline after `else `, so the nested
        // form continues at this level instead of indenting one step per link.
        Some((_, e)) if else_is_chain => sep
            .clone()
            .concat(Doc::text("else "))
            .concat(e.clone()),
        Some((_, e)) => sep
            .clone()
            .concat(Doc::text("else"))
            .concat(sep.clone().concat(e.clone()).nest_if_broken(INDENT)),
    };

    lead.concat(head.concat(tail).group())
}

/// `with pkgs; body` / `assert c; body`
fn with_or_assert(node: &SyntaxNode) -> Doc {
    let kw = if node.kind() == SyntaxKind::NODE_WITH {
        "with"
    } else {
        "assert"
    };
    // `with pkgs; # note` is a common shape, and dropping that comment was
    // the largest remaining comment-loss class over the fleet corpus
    // (31 files). Comments lead the form rather than vanishing.
    let mut parts: Vec<Doc> = Vec::new();
    let mut trailing = Doc::nil();
    for p in pieces(node) {
        match p {
            Piece::Node(n) => parts.push(expr(&n)),
            // The comment sits between `with X;` and the body, which is where
            // it stays. An earlier cut HOISTED it above the keyword; that is a
            // RELOCATION, and a relocation is not a fixed point — the second
            // pass sees a comment attached to a different position and moves
            // it again. Idempotence over the fleet corpus fell from 99.67% to
            // 97.57% on exactly that change, which is how it was found.
            Piece::Comment(c) => trailing = trailing.concat(Doc::text(c)).concat(Doc::hardline()),
            _ => {}
        }
    }
    let mut it = parts.into_iter();
    let subject = it.next().unwrap_or_else(Doc::nil);
    let body = it.next().unwrap_or_else(Doc::nil);
    Doc::text(kw)
        .concat(Doc::text(" "))
        .concat(subject)
        .concat(Doc::text(";"))
        .concat(Doc::hardline())
        .concat(trailing)
        .concat(body)
}

/// Every `NODE_*` kind the renderer reaches via the catch-all arm.
/// The coverage test asserts this is empty over the corpus.
pub fn unhandled_kinds(src: &str) -> Vec<SyntaxKind> {
    use SyntaxKind::*;
    const HANDLED: &[SyntaxKind] = &[
        NODE_ATTR_SET, NODE_LEGACY_LET, NODE_LIST, NODE_LET_IN, NODE_ATTRPATH_VALUE,
        NODE_INHERIT, NODE_PAREN, NODE_IF_ELSE, NODE_WITH, NODE_ASSERT, NODE_LAMBDA,
        NODE_APPLY, NODE_BIN_OP, NODE_UNARY_OP, NODE_HAS_ATTR, NODE_SELECT, NODE_ATTRPATH,
        NODE_STRING, NODE_INTERPOL, NODE_DYNAMIC, NODE_IDENT, NODE_LITERAL, NODE_PATH_ABS,
        NODE_PATH_REL, NODE_PATH_HOME, NODE_PATH_SEARCH, NODE_CUR_POS, NODE_ROOT,
        NODE_PATTERN, NODE_PAT_ENTRY, NODE_PAT_BIND, NODE_INHERIT_FROM, NODE_IDENT_PARAM,
    ];
    let mut out: Vec<SyntaxKind> = rnix::Root::parse(src)
        .syntax()
        .descendants()
        .map(|n| n.kind())
        .filter(|k| !HANDLED.contains(k))
        .collect();
    out.sort_by_key(|k| *k as u16);
    out.dedup();
    out
}
