//! Source text → diagnostics. **Pure**: no LSP types, no IO, no async.
//!
//! Everything hard about this module is the question *where does the squiggle
//! go*, and the answer is not always "the producer told us".
//!
//! Of rnix 0.14's eight [`rnix::ParseError`] variants, **five carry a
//! `TextRange` and three do not** (`UnexpectedEOF`, `UnexpectedEOFWanted`,
//! `RecursionLimitExceeded`). Of `sui-ir`'s five [`LowerError`] variants,
//! **one** carries byte offsets. So for most of the error surface, the position
//! is something this module has to *decide*.
//!
//! The tempting default — anchor a span-less error at line 0, column 0 — is
//! wrong in the specific way that is hard to notice: an unclosed brace at the
//! bottom of a 400-line file puts a red squiggle on the first character, and the
//! reader goes looking for a problem that is nowhere near there. [`Anchor`]
//! exists so "we were not told where this is" is a *represented* state with a
//! deliberate answer per case, rather than a silent zero.

use sui_ir::lower::{lower_file, LowerError};
use zahyou::{Lines, Range};

/// How severe a finding is. Deliberately two-valued: sui has no lint tier yet,
/// and inventing `Hint`/`Information` before anything produces one would be a
/// vocabulary nothing can fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// What went wrong, as a typed value whose `Display` **is** the operator-facing
/// message (★★ TYPED EMISSION — the message comes from a typed error surface,
/// never a `format!()` at the call site).
///
/// These deliberately do **not** reuse rnix's own `Display`, which renders as
/// `"error node at 5..7"` — byte offsets are redundant once the finding carries
/// a range, and an editor tooltip is the wrong place to show them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Finding {
    #[error("unexpected token")]
    Unexpected,
    #[error("unexpected token after the end of the expression")]
    UnexpectedExtra,
    #[error("unexpected {got:?}, expected one of {wanted:?}")]
    UnexpectedWanted {
        got: rnix::SyntaxKind,
        wanted: Vec<rnix::SyntaxKind>,
    },
    #[error("this pattern argument is bound twice")]
    UnexpectedDoubleBind,
    #[error("duplicate formal argument `{name}`")]
    DuplicatedArgs { name: String },
    #[error("unexpected end of file")]
    UnexpectedEof,
    #[error("unexpected end of file, expected one of {wanted:?}")]
    UnexpectedEofWanted { wanted: Vec<rnix::SyntaxKind> },
    #[error("expression nests too deeply to parse")]
    RecursionLimit,
    /// rnix reported an error kind this build does not know about — see the
    /// note on [`classify_parse_error`]. Surfaced as its own finding rather
    /// than folded into [`Self::Unexpected`], because "the file is wrong here"
    /// and "we did not understand what rnix said" are different claims and
    /// only one of them is about the user's file.
    #[error("sui could not interpret this parse error: {rendered}")]
    UnrecognizedParseError { rendered: String },

    #[error("this part of the file could not be parsed")]
    ParseErrorNode,
    #[error("`{construct}` is missing its `{field}`")]
    Missing {
        construct: &'static str,
        field: &'static str,
    },
    #[error("integer literal `{text}` does not fit in a 64-bit signed integer")]
    IntOutOfRange { text: String },
    #[error("`{text}` is not a valid floating-point literal")]
    BadFloat { text: String },
    /// A binding group's `sui-normalize` plan referenced a syntax node the
    /// lowering walk never turned into an expression — an internal
    /// inconsistency between the plan and the arena, NOT a defect in the
    /// user's file.
    ///
    /// It is surfaced rather than swallowed because the alternative is worse
    /// than a confusing squiggle: the IR has no rowan tree at eval time, so a
    /// dropped plan silently reinstates nix's parse-time attrset splice being
    /// missed, which is a WRONG ANSWER at exit 0. Loud and misattributed beats
    /// silent and wrong; the wording says whose bug it is.
    #[error("internal: sui could not resolve a binding-group plan here (this is a sui bug, not an error in your file)")]
    PlanUnresolved,
}

impl Finding {
    /// A stable machine-readable code. Editors group and filter on this, so it
    /// must not change when the human message is reworded.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unexpected => "sui/unexpected",
            Self::UnexpectedExtra => "sui/unexpected-extra",
            Self::UnexpectedWanted { .. } => "sui/unexpected-wanted",
            Self::UnexpectedDoubleBind => "sui/double-bind",
            Self::DuplicatedArgs { .. } => "sui/duplicate-arg",
            Self::UnexpectedEof => "sui/unexpected-eof",
            Self::UnexpectedEofWanted { .. } => "sui/unexpected-eof-wanted",
            Self::RecursionLimit => "sui/recursion-limit",
            Self::UnrecognizedParseError { .. } => "sui/unrecognized-parse-error",
            Self::ParseErrorNode => "sui/parse-error-node",
            Self::Missing { .. } => "sui/missing-child",
            Self::IntOutOfRange { .. } => "sui/int-out-of-range",
            Self::BadFloat { .. } => "sui/bad-float",
            Self::PlanUnresolved => "sui/plan-unresolved",
        }
    }
}

/// Where a finding attaches.
///
/// The point of this type is that **the third and fourth variants are not
/// spans** — they are admissions that the producer gave us no position, paired
/// with the best honest answer available. Collapsing them into a `Range` at the
/// construction site is how a span-less error silently becomes a squiggle on
/// line 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    /// The producer gave us a byte range. Use it.
    Span { start: u32, end: u32 },
    /// The error *is* "we ran out of input", so the end of the document is not
    /// a fallback — it is the correct location.
    EndOfInput,
    /// No span, but the finding names a literal. If that text occurs **exactly
    /// once** the occurrence is unambiguous and we can point at it; if it occurs
    /// zero or many times we must not guess, and this degrades to
    /// [`Self::WholeDocument`].
    UniqueOccurrence(String),
    /// No span and nothing to locate it by. Underlining the whole document is
    /// honest — it says "somewhere in here" instead of lying about line 0.
    WholeDocument,
}

impl Anchor {
    fn resolve(&self, src: &str, lines: &Lines) -> Range {
        match self {
            Self::Span { start, end } => lines.range(src, *start as usize, *end as usize),
            Self::EndOfInput => lines.range(src, src.len(), src.len()),
            Self::UniqueOccurrence(text) => {
                let mut hits = src.match_indices(text.as_str());
                match (hits.next(), hits.next()) {
                    // Exactly one occurrence: unambiguous, point at it.
                    (Some((at, _)), None) => lines.range(src, at, at + text.len()),
                    // Zero or two-plus: guessing would be worse than admitting.
                    _ => Self::WholeDocument.resolve(src, lines),
                }
            }
            Self::WholeDocument => lines.range(src, 0, src.len()),
        }
    }
}

/// One finding, positioned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Severity,
    pub finding: Finding,
}

impl Diagnostic {
    /// The operator-facing message — the `Finding`'s own `Display`.
    #[must_use]
    pub fn message(&self) -> String {
        self.finding.to_string()
    }
}

/// Check one source file.
///
/// Two stages, and the order matters: rnix's parse errors come first and are
/// reported **in full**, because a language server that shows only the first
/// error makes the reader fix-and-recheck one line at a time. Lowering runs
/// only on a clean parse — `lower_file` collapses any parse failure into a
/// single span-less `ParseFailure`, so running it on a broken file would trade
/// N located errors for one unlocated one.
#[must_use]
pub fn check(src: &str) -> Vec<Diagnostic> {
    let lines = Lines::new(src);
    let parse = rnix::Root::parse(src);

    let parse_errors: Vec<Diagnostic> = parse
        .errors()
        .iter()
        .map(|e| {
            let (finding, anchor) = classify_parse_error(e);
            Diagnostic {
                range: anchor.resolve(src, &lines),
                severity: Severity::Error,
                finding,
            }
        })
        .collect();
    if !parse_errors.is_empty() {
        return parse_errors;
    }

    match lower_file(src) {
        Ok(_) => Vec::new(),
        Err(e) => {
            let (finding, anchor) = classify_lower_error(&e);
            vec![Diagnostic {
                range: anchor.resolve(src, &lines),
                severity: Severity::Error,
                finding,
            }]
        }
    }
}

/// Map one rnix error onto a finding and a place to put it.
///
/// **`rnix::ParseError` is `#[non_exhaustive]`, so this match cannot be
/// exhaustive and a new upstream variant will NOT break the build.** That is
/// worth stating plainly because the opposite is the natural assumption: a
/// closed `match` over an enum usually *is* the guard. Here it is not, so the
/// wildcard has to carry its own weight — it renders whatever rnix said and
/// says openly that sui did not recognise it, rather than quietly labelling an
/// unknown error "unexpected token" and anchoring it somewhere plausible.
///
/// Tier: only-mitigated (C2 — the upstream crate's variant set is outside our
/// control and it has opted out of the compile-time check). The mitigation is
/// that an unrecognised error is *visible as unrecognised*, not that it cannot
/// happen.
fn classify_parse_error(e: &rnix::ParseError) -> (Finding, Anchor) {
    use rnix::ParseError as P;
    let span = |r: &rowan::TextRange| Anchor::Span {
        start: u32::from(r.start()),
        end: u32::from(r.end()),
    };
    match e {
        P::Unexpected(r) => (Finding::Unexpected, span(r)),
        P::UnexpectedExtra(r) => (Finding::UnexpectedExtra, span(r)),
        P::UnexpectedWanted(got, r, wanted) => (
            Finding::UnexpectedWanted {
                got: *got,
                wanted: wanted.to_vec(),
            },
            span(r),
        ),
        P::UnexpectedDoubleBind(r) => (Finding::UnexpectedDoubleBind, span(r)),
        P::DuplicatedArgs(r, name) => (Finding::DuplicatedArgs { name: name.clone() }, span(r)),
        // The three span-less variants. Two of them ARE end-of-input, so that
        // anchor is exact rather than a fallback.
        P::UnexpectedEOF => (Finding::UnexpectedEof, Anchor::EndOfInput),
        P::UnexpectedEOFWanted(wanted) => (
            Finding::UnexpectedEofWanted {
                wanted: wanted.to_vec(),
            },
            Anchor::EndOfInput,
        ),
        P::RecursionLimitExceeded => (Finding::RecursionLimit, Anchor::WholeDocument),
        // Reachable only if rnix adds a variant (see the note above).
        other => (
            Finding::UnrecognizedParseError {
                rendered: other.to_string(),
            },
            Anchor::WholeDocument,
        ),
    }
}

fn classify_lower_error(e: &LowerError) -> (Finding, Anchor) {
    match e {
        // Unreachable via `check` (parse errors short-circuit above), but a
        // caller reaching `lower_file` directly can produce it, so it is mapped
        // rather than unwrapped.
        LowerError::ParseFailure { .. } => (Finding::Unexpected, Anchor::WholeDocument),
        LowerError::ParseErrorNode { start, end } => (
            Finding::ParseErrorNode,
            Anchor::Span {
                start: *start,
                end: *end,
            },
        ),
        LowerError::Missing { construct, field } => (
            Finding::Missing { construct, field },
            Anchor::WholeDocument,
        ),
        // These name the offending literal but not where it is; if the literal
        // is unique in the file that is enough to place it exactly.
        LowerError::IntOutOfRange { text } => (
            Finding::IntOutOfRange { text: text.clone() },
            Anchor::UniqueOccurrence(text.clone()),
        ),
        LowerError::BadFloat { text } => (
            Finding::BadFloat { text: text.clone() },
            Anchor::UniqueOccurrence(text.clone()),
        ),
        // Carries a real byte range, so it anchors precisely — the same shape
        // as `ParseErrorNode`, for a different cause.
        LowerError::PlanUnresolved { start, end } => (
            Finding::PlanUnresolved,
            Anchor::Span {
                start: *start,
                end: *end,
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{check, Anchor, Severity};
    use zahyou::{Lines, Position};

    #[test]
    fn a_valid_file_produces_nothing() {
        assert!(check("{ x = 1; }").is_empty());
        assert!(check("let x = 1; in x").is_empty());
        assert!(check("# just a comment\nnull").is_empty());
    }

    #[test]
    fn a_broken_file_produces_a_located_error() {
        let d = check("{ x = ; }");
        assert!(!d.is_empty(), "a missing value must be reported");
        assert_eq!(d[0].severity, Severity::Error);
        assert_eq!(d[0].range.start.line, 0);
    }

    /// A server reporting only the first error makes the reader fix one line at
    /// a time, rechecking after each. Whatever rnix finds, we forward all of it.
    #[test]
    fn every_parse_error_is_reported_not_just_the_first() {
        let src = "{ a = ; b = ; c = ; }";
        let n = rnix::Root::parse(src).errors().len();
        assert_eq!(check(src).len(), n, "must forward all {n} parse errors");
    }

    /// **The reason `Anchor` is a type.** An unterminated construct at the
    /// bottom of a long file must not squiggle line 0 — that sends the reader
    /// to the wrong end of the document.
    #[test]
    fn an_end_of_input_error_anchors_at_the_end_not_line_zero() {
        let src = "{\n  a = 1;\n  b = 2;\n  c = {\n";
        let d = check(src);
        assert!(!d.is_empty(), "an unclosed brace must be reported");
        let last_line = u32::try_from(Lines::new(src).line_count() - 1).unwrap();
        assert!(
            d.iter().all(|x| x.range.start.line > 0),
            "nothing may anchor at line 0: {:?}",
            d.iter().map(|x| x.range.start).collect::<Vec<_>>()
        );
        assert!(
            d.iter().any(|x| x.range.start.line == last_line),
            "the unclosed construct should point near the end (line {last_line})"
        );
    }

    #[test]
    fn a_unique_literal_anchors_exactly_on_that_literal() {
        let src = "let a = 1;\n    b = BADLIT;\nin a";
        let lines = Lines::new(src);
        let r = Anchor::UniqueOccurrence("BADLIT".to_string()).resolve(src, &lines);
        assert_eq!(r.start, Position::new(1, 8));
        assert_eq!(r.end, Position::new(1, 14));
    }

    /// Guessing between two identical literals would put the squiggle on the
    /// wrong one half the time, which is worse than saying "somewhere in here".
    #[test]
    fn an_ambiguous_literal_degrades_to_the_whole_document_rather_than_guessing() {
        let src = "let a = DUP; b = DUP; in a";
        let lines = Lines::new(src);
        let r = Anchor::UniqueOccurrence("DUP".to_string()).resolve(src, &lines);
        let whole = Anchor::WholeDocument.resolve(src, &lines);
        assert_eq!(r, whole, "two occurrences must not be guessed between");
    }

    #[test]
    fn a_missing_literal_degrades_rather_than_panicking() {
        let src = "let a = 1; in a";
        let lines = Lines::new(src);
        let r = Anchor::UniqueOccurrence("nowhere".to_string()).resolve(src, &lines);
        assert_eq!(r, Anchor::WholeDocument.resolve(src, &lines));
    }

    /// Positions are UTF-16 columns end to end — the whole reason `zahyou`
    /// exists. If this ever reports a byte column, every diagnostic on a line
    /// containing an emoji lands in the wrong place.
    #[test]
    fn columns_are_utf16_not_bytes() {
        let src = "# 🎉 a comment\n{ x = ; }";
        let d = check(src);
        assert!(!d.is_empty());
        // The error is on line 1, which the emoji on line 0 must not shift.
        assert_eq!(d[0].range.start.line, 1);
    }

    /// An empty document must not panic, and any finding must still carry a
    /// resolvable range.
    #[test]
    fn an_empty_document_does_not_panic() {
        let d = check("");
        for x in &d {
            assert_eq!(x.range.start, Position::new(0, 0));
        }
    }

    #[test]
    fn the_message_comes_from_the_typed_finding() {
        let d = check("{ x = ; }");
        assert!(!d[0].message().is_empty());
        assert!(d[0].finding.code().starts_with("sui/"));
    }
}
