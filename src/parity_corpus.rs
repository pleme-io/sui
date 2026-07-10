//! Typed Nix-expression corpus generator for the sealed parity gate.
//!
//! CLOSED-LOOP MASS-SYNTHESIS applied to sui↔nix eval parity: instead of
//! hand-writing escaped Nix strings, we build eval-surface shapes from the
//! canonical typed Nix AST (`gen_nix::NixValue`, `theory/NIX-AST.md`) and
//! render each to canonical Nix source, then byte-check every generated row
//! against the nix oracle. A new eval-surface variant becomes a generated
//! row, not a hand-authored probe — and every Match row can never silently
//! regress. Per TYPED EMISSION, no `format!()` of Nix syntax at call sites —
//! expressions are built from typed `NixValue` and rendered by `gen-nix`.
//!
//! This consumes the real fleet framework `gen-nix` (published on crates.io),
//! not a local mirror — the AST + pretty-printer are owned once in the `gen`
//! repo and reused here (Prime Directive: reuse the primitive, don't fork it).

use gen_nix::ast::{AttrSetEntry, LetBinding, NixBinOp, NixValue, StrPart};

/// Whether a generated row is expected to be byte-identical to nix (`Match`)
/// or is a tracked frontier that a fix must graduate (`KnownDiverge`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RowExpect {
    Match,
    #[allow(dead_code)]
    KnownDiverge,
}

/// One generated corpus row: a stable name, the rendered Nix expression, and
/// its expected verdict.
pub struct CorpusRow {
    pub name: String,
    pub expr: String,
    pub expect: RowExpect,
}

// ── small typed builders over gen_nix::NixValue ───────────────────────────

fn str_(s: &str) -> NixValue {
    NixValue::Str(s.to_string())
}

/// `a.b.c` select / attrpath — used for ident-rooted selects (bind a
/// derivation to a name in a `let`, then select `name.drvPath`).
fn path(parts: &[&str]) -> NixValue {
    NixValue::AttrPath(parts.iter().map(|s| s.to_string()).collect())
}

/// `f a b …`
fn apply(func: NixValue, args: Vec<NixValue>) -> NixValue {
    NixValue::Apply {
        func: Box::new(func),
        args,
    }
}

/// `left + right`
fn add(left: NixValue, right: NixValue) -> NixValue {
    NixValue::BinOp {
        op: NixBinOp::Add,
        left: Box::new(left),
        right: Box::new(right),
    }
}

/// `"${e}<lit>…"`
fn interp(parts: Vec<StrPart>) -> NixValue {
    NixValue::InterpolatedStr(parts)
}

/// `let <name> = <value>; … in <body>`
fn let_(bindings: Vec<(&str, NixValue)>, body: NixValue) -> NixValue {
    NixValue::Let {
        bindings: bindings
            .into_iter()
            .map(|(name, value)| LetBinding::Bind {
                name: name.to_string(),
                value,
            })
            .collect(),
        body: Box::new(body),
    }
}

/// A non-recursive attrset from typed entries (already-built `AttrSetEntry`s,
/// so dotted keys + full-set keys can mix — the merge cases need that).
fn attrs(entries: Vec<AttrSetEntry>) -> NixValue {
    NixValue::AttrSet {
        recursive: false,
        entries,
    }
}

/// `derivation { name = "<name>"; system = builtins.currentSystem;
///               builder = "/bin/sh"; <extra> }`
fn drv(name: &str, extra: Vec<AttrSetEntry>) -> NixValue {
    let mut entries = vec![
        gen_nix::ast::entry("name", str_(name)),
        gen_nix::ast::entry("system", NixValue::Raw("builtins.currentSystem".to_string())),
        gen_nix::ast::entry("builder", str_("/bin/sh")),
    ];
    entries.extend(extra);
    apply(NixValue::Ident("derivation".to_string()), vec![attrs(entries)])
}

fn row(name: &str, expr: NixValue, expect: RowExpect) -> CorpusRow {
    CorpusRow {
        name: name.to_string(),
        expr: expr.render_to_string(),
        expect,
    }
}

/// Generate the mass-synthesis parity matrix. Every row is byte-checked
/// sui-vs-nix by the caller, grouped by the eval-surface category each root
/// this arc hardened, plus close variants that guard the *class*.
pub fn generate() -> Vec<CorpusRow> {
    use gen_nix::ast::{dotted_entry, entry};
    let mut rows: Vec<CorpusRow> = Vec::new();

    // ── attrset dotted + full-set deep-merge (root 73b904d) ───────────────
    // order-1: dotted then full-set — MUST merge (the pkg-config-wrapper
    // env.addFlags drop). s.a.b + s.a.c == "xy".
    rows.push(row(
        "attr-merge order1 (dotted then fullset)",
        let_(
            vec![(
                "s",
                attrs(vec![
                    dotted_entry("a.b", str_("x")),
                    entry("a", attrs(vec![entry("c", str_("y"))])),
                ]),
            )],
            add(path(&["s", "a", "b"]), path(&["s", "a", "c"])),
        ),
        RowExpect::Match,
    ));
    // deep-nested collision — a.b.c + a.b.e + a.d == "132".
    rows.push(row(
        "attr-merge deep-nested",
        let_(
            vec![(
                "s",
                attrs(vec![
                    dotted_entry("a.b.c", str_("1")),
                    entry("a", attrs(vec![entry("d", str_("2"))])),
                    dotted_entry("a.b.e", str_("3")),
                ]),
            )],
            add(
                add(path(&["s", "a", "b", "c"]), path(&["s", "a", "b", "e"])),
                path(&["s", "a", "d"]),
            ),
        ),
        RowExpect::Match,
    ));
    // non-colliding control — a plain attrset is unchanged by the merge path.
    rows.push(row(
        "attr-merge non-colliding control",
        let_(
            vec![(
                "s",
                attrs(vec![
                    entry("a", attrs(vec![entry("b", str_("x"))])),
                    entry("c", str_("z")),
                ]),
            )],
            add(path(&["s", "a", "b"]), path(&["s", "c"])),
        ),
        RowExpect::Match,
    ));

    // ── concatStringsSep/concatStrings context (root a67c244) ─────────────
    // A multi-output element's `out` must survive into the drv input set.
    rows.push(row(
        "concatStringsSep multi-output context",
        let_(
            vec![
                (
                    "m",
                    drv("m", vec![entry("outputs", NixValue::List(vec![str_("out"), str_("dev")]))]),
                ),
                (
                    "c",
                    drv(
                        "c",
                        vec![entry(
                            "L",
                            apply(
                                path(&["builtins", "concatStringsSep"]),
                                vec![
                                    str_(":"),
                                    NixValue::List(vec![
                                        interp(vec![
                                            StrPart::Interp(path(&["m", "out"])),
                                            StrPart::Literal("/lib".to_string()),
                                        ]),
                                        interp(vec![
                                            StrPart::Interp(path(&["m", "dev"])),
                                            StrPart::Literal("/include".to_string()),
                                        ]),
                                    ]),
                                ],
                            ),
                        )],
                    ),
                ),
            ],
            path(&["c", "drvPath"]),
        ),
        RowExpect::Match,
    ));
    // Empty-separator concatStringsSep (the concatStrings shape) preserves
    // context the same way. (`builtins.concatStrings` is not a nix builtin —
    // it is `lib.concatStrings` — so the no-separator path is exercised via
    // `concatStringsSep ""`, which both engines expose.)
    rows.push(row(
        "concatStringsSep empty-sep single-element context",
        let_(
            vec![
                (
                    "m",
                    drv("m2", vec![entry("outputs", NixValue::List(vec![str_("out"), str_("dev")]))]),
                ),
                (
                    "c",
                    drv(
                        "c2",
                        vec![entry(
                            "L",
                            apply(
                                path(&["builtins", "concatStringsSep"]),
                                vec![
                                    str_(""),
                                    NixValue::List(vec![interp(vec![
                                        StrPart::Interp(path(&["m", "out"])),
                                        StrPart::Literal("/lib".to_string()),
                                    ])]),
                                ],
                            ),
                        )],
                    ),
                ),
            ],
            path(&["c", "drvPath"]),
        ),
        RowExpect::Match,
    ));

    // ── multi-output modulo (the class fixed earlier this arc) ────────────
    // A consumer selecting a specific output of a multi-output producer.
    rows.push(row(
        "multi-output producer .dev drvPath",
        let_(
            vec![
                (
                    "p",
                    drv(
                        "prod",
                        vec![entry(
                            "outputs",
                            NixValue::List(vec![str_("out"), str_("dev"), str_("lib")]),
                        )],
                    ),
                ),
                (
                    "cons",
                    drv(
                        "cons",
                        vec![entry(
                            "BI",
                            interp(vec![StrPart::Interp(path(&["p", "dev"]))]),
                        )],
                    ),
                ),
            ],
            path(&["cons", "drvPath"]),
        ),
        RowExpect::Match,
    ));

    rows
}
