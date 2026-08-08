//! The Wadler/Oppen pretty-printing algebra.
//!
//! Ported verbatim in behaviour from `blue-lang-fmt::doc` — this is the
//! crate that becomes the shared fleet library. Keeping it byte-compatible
//! with blue's semantics is the point: two formatters that disagree about
//! what `group` means are two canonical forms, and the fleet only gets one.
//!
//! `group` is the ONLY decision point. Given a document and a width,
//! exactly one output exists.

use std::rc::Rc;

#[derive(Clone, Debug)]
pub enum Doc {
    Nil,
    Text(Rc<str>),
    /// Break candidate. `flat` is what it renders as when the enclosing
    /// group fits on one line.
    Line { flat: &'static str },
    /// A break that NOTHING can flatten — a comment ends its line, always.
    /// blue has no equivalent because its trivia never reached the Doc;
    /// with a lossless CST it does, and a `//` comment followed by a flat
    /// `line` would swallow the rest of the expression into the comment.
    HardLine,
    Concat(Rc<Doc>, Rc<Doc>),
    Nest(isize, Rc<Doc>),
    /// Indent ONLY if the enclosing group broke. nixfmt indents an absorbed
    /// term relative to the LINE it hugged onto, not to the logical nesting.
    NestIfBroken(isize, Rc<Doc>),
    Group(Rc<Doc>),
    /// Transparent at print time; in `fits`, a hard break inside it ENDS the
    /// measurement successfully instead of forbidding flatness.
    Absorb(Rc<Doc>),
}

impl Doc {
    pub fn nil() -> Self {
        Doc::Nil
    }

    pub fn text(s: impl Into<Rc<str>>) -> Self {
        Doc::Text(s.into())
    }

    /// A space when flat; a newline when broken.
    pub fn line() -> Self {
        Doc::Line { flat: " " }
    }

    /// Nothing when flat; a newline when broken.
    pub fn softline() -> Self {
        Doc::Line { flat: "" }
    }

    /// Always a newline. Poisons every enclosing group into Break mode.
    pub fn hardline() -> Self {
        Doc::HardLine
    }

    pub fn concat(self, other: Doc) -> Self {
        match (&self, &other) {
            (Doc::Nil, _) => other,
            (_, Doc::Nil) => self,
            _ => Doc::Concat(Rc::new(self), Rc::new(other)),
        }
    }

    pub fn nest(self, indent: isize) -> Self {
        Doc::Nest(indent, Rc::new(self))
    }

    pub fn nest_if_broken(self, indent: isize) -> Self {
        Doc::NestIfBroken(indent, Rc::new(self))
    }

    pub fn group(self) -> Self {
        Doc::Group(Rc::new(self))
    }

    pub fn absorb(self) -> Self {
        Doc::Absorb(Rc::new(self))
    }

    pub fn join(docs: impl IntoIterator<Item = Doc>, sep: Doc) -> Doc {
        let mut out = Doc::Nil;
        let mut first = true;
        for d in docs {
            if !first {
                out = out.concat(sep.clone());
            }
            out = out.concat(d);
            first = false;
        }
        out
    }

    pub fn concat_all(docs: impl IntoIterator<Item = Doc>) -> Doc {
        let mut out = Doc::Nil;
        for d in docs {
            out = out.concat(d);
        }
        out
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Break,
    /// `fits`-only: inside an absorbed term. A hard break here means "the
    /// first line ended", i.e. SUCCESS, not "cannot be flat".
    Absorbed,
}

/// Render `doc` at `width` columns.
pub fn pretty(doc: &Doc, width: usize) -> String {
    let mut out = String::new();
    let mut stack: Vec<(isize, Mode, Doc)> = vec![(0, Mode::Break, doc.clone())];
    let mut col: usize = 0;
    // MEASURED (nixfmt 1.3.1): the fit budget does NOT charge the line's
    // leading indentation. `{ a = <95 b>; }` stays flat at attrset depth 1
    // AND depth 3 -- same n, six columns of indent apart -- and nixfmt emits
    // a 108-column line rather than break.
    let mut line_indent: usize = 0;

    while let Some((indent, mode, d)) = stack.pop() {
        match d {
            Doc::Nil => {}
            Doc::Text(s) => {
                out.push_str(&s);
                // A Text may legitimately contain newlines (an indented
                // string's body is copied verbatim). Column must track the
                // LAST line, or every group after a multi-line literal
                // measures against a phantom column and breaks wrongly.
                match s.rfind('\n') {
                    Some(i) => {
                        col = s[i + 1..].chars().count();
                        line_indent = 0;
                    }
                    None => col += s.chars().count(),
                }
            }
            Doc::HardLine => {
                newline(&mut out, indent, &mut col);
                line_indent = col;
            }
            Doc::Line { flat } => match mode {
                Mode::Flat | Mode::Absorbed => {
                    out.push_str(flat);
                    col += flat.chars().count();
                }
                Mode::Break => {
                    newline(&mut out, indent, &mut col);
                    line_indent = col;
                }
            },
            Doc::Concat(a, b) => {
                stack.push((indent, mode, (*b).clone()));
                stack.push((indent, mode, (*a).clone()));
            }
            Doc::Nest(n, inner) => {
                stack.push((indent + n, mode, (*inner).clone()));
            }
            Doc::NestIfBroken(n, inner) => {
                let i = if mode == Mode::Break { indent + n } else { indent };
                stack.push((i, mode, (*inner).clone()));
            }
            Doc::Absorb(inner) => {
                stack.push((indent, mode, (*inner).clone()));
            }
            Doc::Group(inner) => {
                let used = col.saturating_sub(line_indent);
                let m = if fits(width.saturating_sub(used), &inner, &stack) {
                    Mode::Flat
                } else {
                    Mode::Break
                };
                stack.push((indent, m, (*inner).clone()));
            }
        }
    }
    out
}

/// Emit a break, trimming the indent this printer itself just wrote when the
/// line turned out to be empty.
///
/// **The trim MUST live here and not in a post-pass over the finished text.**
/// A post-pass cannot tell an indent it emitted from the interior of an
/// indented string, and a Nix `''...''` body is SEMANTIC — its trailing
/// spaces are part of the value. Stripping them changed the program in 3
/// fleet files (a nginx config, a CoreDNS zone, a postgres bootstrap script),
/// each time turning `"…\n    \n    # comment"` into `"…\n\n    # comment"`.
/// Trimming only what `newline` wrote is precise by construction: a newline
/// inside a verbatim `Text` never reaches this function.
fn newline(out: &mut String, indent: isize, col: &mut usize) {
    // Trim ONLY when the whole current line is whitespace this printer wrote
    // as an indent. Trailing spaces after real content are semantic inside a
    // `''` body once its lines are rendered as separate Texts.
    let start = out.rfind('\n').map_or(0, |i| i + 1);
    if out[start..].chars().all(|c| c == ' ') {
        out.truncate(start);
    }
    out.push('\n');
    let pad = indent.max(0) as usize;
    for _ in 0..pad {
        out.push(' ');
    }
    *col = pad;
}

/// Would `doc`, rendered flat, fit in `space` columns?
fn fits(space: usize, doc: &Doc, rest: &[(isize, Mode, Doc)]) -> bool {
    let mut remaining = space as isize;
    // `nested` = we have descended into a SUBGROUP. An `Absorb` marker is
    // honoured only by the group it directly belongs to; a group further out
    // still sees the hard break and must break. Without this the marker leaks
    // outward and a 1-item attrset holding a multi-line string stays flat.
    let mut local: Vec<(Mode, bool, Doc)> = vec![(Mode::Flat, false, doc.clone())];
    let mut tail_idx = rest.len();
    let mut in_tail = false;

    loop {
        let (mode, nested, d) = match local.pop() {
            Some(x) => x,
            None => {
                if tail_idx == 0 {
                    return remaining >= 0;
                }
                in_tail = true;
                tail_idx -= 1;
                let (_, m, d) = &rest[tail_idx];
                (*m, true, d.clone())
            }
        };
        if remaining < 0 {
            return false;
        }
        match d {
            Doc::Nil => {}
            Doc::Text(s) => {
                // A Text containing a newline can never fit flat.
                if s.contains('\n') {
                    return false;
                }
                remaining -= s.chars().count() as isize;
            }
            // A hard break inside a candidate group means the group CANNOT
            // be flat. Returning `false` is what propagates "this contains a
            // comment" outward, which is the whole mechanism keeping a
            // comment on its own line.
            Doc::HardLine => {
                if in_tail || mode == Mode::Absorbed {
                    return remaining >= 0;
                }
                return false;
            }
            Doc::Line { flat } => match mode {
                Mode::Break => return remaining >= 0,
                Mode::Flat | Mode::Absorbed => remaining -= flat.chars().count() as isize,
            },
            Doc::Concat(a, b) => {
                local.push((mode, nested, (*b).clone()));
                local.push((mode, nested, (*a).clone()));
            }
            Doc::Nest(_, inner) | Doc::NestIfBroken(_, inner) => {
                local.push((mode, nested, (*inner).clone()))
            }
            Doc::Absorb(inner) => {
                let m = if nested { mode } else { Mode::Absorbed };
                local.push((m, nested, (*inner).clone()));
            }
            Doc::Group(inner) => {
                let m = if mode == Mode::Absorbed { Mode::Absorbed } else { Mode::Flat };
                local.push((m, true, (*inner).clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Doc {
        Doc::text(s.to_string())
    }

    #[test]
    fn a_group_that_fits_stays_flat() {
        let d = t("a").concat(Doc::line()).concat(t("b")).group();
        assert_eq!(pretty(&d, 80), "a b");
    }

    #[test]
    fn a_group_that_does_not_fit_breaks() {
        let d = t("aaaa").concat(Doc::line()).concat(t("bbbb")).group();
        assert_eq!(pretty(&d, 5), "aaaa\nbbbb");
    }

    #[test]
    fn inner_groups_decide_independently() {
        let inner = t("b").concat(Doc::line()).concat(t("c")).group();
        let d = t("aaaaaaaa").concat(Doc::line()).concat(inner).group();
        assert_eq!(pretty(&d, 10), "aaaaaaaa\nb c");
    }

    /// A hard line forces its enclosing group to break even when the text
    /// would have fitted. This is what keeps a comment from swallowing code.
    #[test]
    fn a_hardline_poisons_the_enclosing_group() {
        let d = t("a").concat(Doc::hardline()).concat(t("b")).group();
        assert_eq!(pretty(&d, 80), "a\nb");
    }

    /// Anti-vacuity: width must actually matter.
    #[test]
    fn width_changes_the_output() {
        let d = t("aaa").concat(Doc::line()).concat(t("bbb")).group();
        assert_ne!(pretty(&d, 80), pretty(&d, 3));
    }

    /// A multi-line Text must not corrupt the column, or every following
    /// group measures against a phantom column.
    #[test]
    fn a_multiline_text_resets_the_column() {
        let d = t("''\n  body\n''")
            .concat(Doc::line())
            .concat(t("xx"))
            .group();
        // The group cannot be flat (the text has a newline), so the line breaks.
        assert_eq!(pretty(&d, 80), "''\n  body\n''\nxx");
    }

    #[test]
    fn rendering_is_deterministic() {
        let d = t("x").concat(Doc::line()).concat(t("y")).group();
        let a = pretty(&d, 3);
        for _ in 0..100 {
            assert_eq!(pretty(&d, 3), a);
        }
    }
}
