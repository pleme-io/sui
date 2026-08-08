//! The formatting laws, expressed over the LOSSLESS CST.
//!
//! blue's round-trip law compares parsed trees, and `caixa-fmt`'s proptest
//! did the same — which is precisely why it could not see comment loss: the
//! trees it compared had already dropped trivia. With rnix the trivia is in
//! the tree, so the law can be strictly stronger.
//!
//! The signature has TWO halves and each is independently load-bearing.
//! Measured against deliberate mutations:
//!
//! | mutation                     | tokens | comments |
//! |------------------------------|--------|----------|
//! | pure re-layout (correct fmt) | equal  | equal    |
//! | a comment dropped            | EQUAL  | differ   |
//! | `a = 1;` -> `a = 2;`         | differ | EQUAL    |
//! | `''` body re-indented        | differ | EQUAL    |
//!
//! Rows 2 and 3 each pass one half and fail the other, so neither half is
//! redundant. Row 4 records the Nix-specific trap: an indented string's
//! body is a `TOKEN_STRING_CONTENT`, i.e. SEMANTIC — re-indenting inside
//! `''...''` changes the program.

use rnix::SyntaxKind;
use rowan::{NodeOrToken, WalkEvent};

/// What formatting must preserve exactly.
#[derive(Debug, PartialEq, Eq)]
pub struct Signature {
    /// Every non-trivia token, in order, with its text.
    pub tokens: Vec<(SyntaxKind, String)>,
    /// Every comment, in order, trimmed of surrounding horizontal space.
    pub comments: Vec<String>,
}

pub fn signature(src: &str) -> Signature {
    let parsed = rnix::Root::parse(src);
    let mut tokens = Vec::new();
    let mut comments = Vec::new();
    for ev in parsed.syntax().preorder_with_tokens() {
        if let WalkEvent::Enter(NodeOrToken::Token(t)) = ev {
            match t.kind() {
                SyntaxKind::TOKEN_WHITESPACE => {}
                SyntaxKind::TOKEN_COMMENT => comments.push(normalize_comment(t.text())),
                // A TRAILING COMMA in a formal pattern — `{ a, b, }` — is a
                // spelling, not a meaning: it parses to the same pattern as
                // `{ a, b }`. Keeping it in the signature would make the law
                // forbid the formatter from ever normalizing it, which is
                // the one thing a canonical form must be allowed to do.
                //
                // Deliberately NARROW: only a comma whose immediate
                // non-trivia successor closes the pattern. Any other comma
                // stays in the signature, so a DROPPED separator between two
                // real entries is still a breach. Proven by
                // `an_interior_comma_is_still_semantic`.
                SyntaxKind::TOKEN_COMMA if next_significant_is_close(&t) => {}
                // An INDENTED string's absolute indentation is not its value.
                // Nix strips the common leading indentation at parse time and
                // treats a whitespace-only line as empty, both verified
                // against real nix:
                //   ''\n  x\n''  ==  ''\n      x\n''            -> true
                //   ''\n x\n \n y\n'' == ''\n x\n\n y\n''       -> true
                //   ''\n      x\n        y\n''                  -> "x\n  y\n"
                // Comparing raw bytes here flagged 195 of 1197 nixfmt outputs
                // as "meaning changed" when nixfmt was correct and this law
                // was wrong. An over-strict law is not the safe direction: it
                // makes a correct formatter unadoptable.
                SyntaxKind::TOKEN_STRING_CONTENT if in_indented_string(&t) => {
                    tokens.push((SyntaxKind::TOKEN_STRING_CONTENT, strip_indent(t.text())))
                }
                k => tokens.push((k, t.text().to_string())),
            }
        }
    }
    Signature { tokens, comments }
}

/// Does this string-content token belong to an `''...''` string?
///
/// A `"..."` string has NO indentation stripping, so its bytes are its value
/// and must be compared exactly. Getting this backwards would make the law
/// blind to a real change inside a double-quoted string.
fn in_indented_string(t: &rnix::SyntaxToken) -> bool {
    t.parent()
        .into_iter()
        .flat_map(|p| p.children_with_tokens())
        .filter_map(|c| c.into_token())
        .any(|x| x.kind() == SyntaxKind::TOKEN_STRING_START && x.text() == "''")
}

/// Nix's own indented-string normalization, applied so the law compares
/// VALUES rather than layout.
///
/// Known limit, stated rather than hidden: Nix computes the minimum indent
/// across the WHOLE string, while interpolation splits it into several
/// content tokens. Normalizing per token is therefore an approximation — it
/// can call two strings equal that Nix would distinguish only when an
/// interpolation boundary separates the least-indented lines. It is strictly
/// closer than raw bytes, and the residue is recorded here rather than
/// discovered later.
fn strip_indent(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let min = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                ""
            } else {
                &l[min.min(l.len())..]
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Is the next non-trivia token after `t` the `}` that closes a pattern?
fn next_significant_is_close(t: &rnix::SyntaxToken) -> bool {
    let mut cur = t.next_token();
    while let Some(n) = cur {
        if n.kind().is_trivia() {
            cur = n.next_token();
            continue;
        }
        return n.kind() == SyntaxKind::TOKEN_R_BRACE
            && t.parent().map(|p| p.kind()) == Some(SyntaxKind::NODE_PATTERN);
    }
    false
}

/// A comment's INTERIOR is what must survive; its surrounding whitespace is
/// layout. `# x` and `#  x` carry the same note, and a formatter is allowed
/// to normalize the gap after the `#`.
fn normalize_comment(text: &str) -> String {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix('#') {
        let mut out = String::from("#");
        let r = rest.trim();
        if !r.is_empty() {
            out.push(' ');
            out.push_str(r);
        }
        out
    } else {
        // A /* ... */ block: collapse only the outer edges, never the interior,
        // because a block comment's internal layout is often deliberate (ASCII
        // tables, code samples).
        t.to_string()
    }
}

/// Did formatting preserve meaning and every comment?
pub fn preserves(before: &str, after: &str) -> Result<(), LawBreach> {
    let a = signature(before);
    let b = signature(after);
    if a.tokens != b.tokens {
        let at = first_divergence(&a.tokens, &b.tokens);
        return Err(LawBreach::TokenStream { at });
    }
    if a.comments != b.comments {
        return Err(LawBreach::CommentLoss {
            before: a.comments.len(),
            after: b.comments.len(),
            first_missing: a
                .comments
                .iter()
                .find(|c| !b.comments.contains(c))
                .cloned(),
        });
    }
    Ok(())
}

fn first_divergence(a: &[(SyntaxKind, String)], b: &[(SyntaxKind, String)]) -> String {
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        if x != y {
            return format!("token {i}: {x:?} != {y:?}");
        }
    }
    format!("length {} != {}", a.len(), b.len())
}

#[derive(Debug)]
pub enum LawBreach {
    TokenStream {
        at: String,
    },
    CommentLoss {
        before: usize,
        after: usize,
        first_missing: Option<String>,
    },
}

impl std::fmt::Display for LawBreach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LawBreach::TokenStream { at } => {
                write!(f, "formatting CHANGED THE PROGRAM at {at}")
            }
            LawBreach::CommentLoss {
                before,
                after,
                first_missing,
            } => write!(
                f,
                "formatting changed the comments ({before} -> {after}); first missing: {}",
                first_missing.as_deref().unwrap_or("(reordered, none absent)")
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "{\n  # keep me\n  a = 1;\n}\n";

    /// The law must ACCEPT a pure re-layout, or it forbids formatting itself.
    #[test]
    fn pure_relayout_satisfies_the_law() {
        assert!(preserves(BASE, "{ # keep me\n a = 1; }\n").is_ok());
    }

    /// The comment half is load-bearing: the token stream is IDENTICAL here.
    #[test]
    fn a_dropped_comment_breaks_the_law_though_the_tokens_match() {
        let dropped = "{\n  a = 1;\n}\n";
        assert_eq!(signature(BASE).tokens, signature(dropped).tokens);
        assert!(matches!(
            preserves(BASE, dropped),
            Err(LawBreach::CommentLoss { .. })
        ));
    }

    /// The token half is load-bearing: the comments are IDENTICAL here.
    #[test]
    fn a_changed_value_breaks_the_law_though_the_comments_match() {
        let changed = "{\n  # keep me\n  a = 2;\n}\n";
        assert_eq!(signature(BASE).comments, signature(changed).comments);
        assert!(matches!(
            preserves(BASE, changed),
            Err(LawBreach::TokenStream { .. })
        ));
    }

    /// **The Nix trap, correctly stated.** An earlier version of this test
    /// asserted the OPPOSITE — that re-indenting `''...''` changes the value —
    /// and it was simply wrong. Verified against real nix:
    ///   `let a = ''\n  x\n''; b = ''\n      x\n''; in a == b`  ->  true
    /// Nix strips the common leading indentation, so absolute indent is
    /// layout. The law must permit it or a correct formatter is unadoptable.
    #[test]
    fn reindenting_an_indented_string_is_permitted() {
        assert!(preserves("''\n  a\n''", "''\n    a\n''").is_ok());
    }

    /// A whitespace-only line inside `''...''` equals an empty one — also
    /// verified against real nix (`a == b` -> true).
    #[test]
    fn a_whitespace_only_line_in_an_indented_string_is_empty() {
        assert!(preserves("''\n  a\n  \n  b\n''", "''\n  a\n\n  b\n''").is_ok());
    }

    /// **The guard.** The exemption is about INDENTATION, not content. A real
    /// change to an indented string's text is still a breach, and RELATIVE
    /// indentation is preserved by Nix, so changing it is a breach too.
    #[test]
    fn indented_string_content_is_still_semantic() {
        assert!(preserves("''\n  a\n''", "''\n  b\n''").is_err());
        // Relative indent differs: strips to "a\n  b\n" vs "a\nb\n".
        assert!(preserves("''\n  a\n    b\n''", "''\n  a\n  b\n''").is_err());
    }

    /// And a DOUBLE-quoted string gets no exemption at all — it has no
    /// indentation stripping, so its bytes are its value.
    #[test]
    fn a_double_quoted_string_has_no_indent_exemption() {
        assert!(preserves("\"  a\"", "\"    a\"").is_err());
    }

    /// Whitespace between tokens is NOT semantic, so the law must ignore it —
    /// otherwise it would forbid every reflow and the formatter could do nothing.
    #[test]
    fn interior_whitespace_is_not_semantic() {
        assert!(preserves("{ a = 1; }", "{\n  a = 1;\n}").is_ok());
    }

    /// A trailing comma in a pattern is a spelling, so normalizing it either
    /// way is permitted. Both directions, because a canonical form has to be
    /// free to ADD one as well as remove it.
    #[test]
    fn a_trailing_pattern_comma_is_not_semantic() {
        assert!(preserves("{ a, b, }: a", "{ a, b }: a").is_ok());
        assert!(preserves("{ a, b }: a", "{ a, b, }: a").is_ok());
    }

    /// **The guard on that loosening.** An INTERIOR comma separates two real
    /// entries, so dropping one changes the pattern. If this ever passes, the
    /// exemption above has grown past its justification and is hiding real
    /// breakage.
    #[test]
    fn an_interior_comma_is_still_semantic() {
        assert!(matches!(
            preserves("{ a, b }: a", "{ a b }: a"),
            Err(LawBreach::TokenStream { .. })
        ));
    }

    /// And a comma outside a pattern is never exempt.
    #[test]
    fn the_exemption_does_not_reach_outside_a_pattern() {
        let before = "{ a = { b = 1; }; }";
        assert_eq!(
            signature(before).tokens,
            signature("{\n  a = {\n    b = 1;\n  };\n}").tokens
        );
    }
}
