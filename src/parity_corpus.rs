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

    // ── inner dynamic attrpath key laziness (root: eval.rs deferred tail) ──
    // CppNix defers a dynamic key that is NOT at the head of an attrpath:
    // `{ a.${e} = v; }` builds `{ a = <thunk {${e}=v}>; }`, so `e` never
    // forces until `.a` is demanded. Reading a SIBLING (`.other`) must not
    // force the inner dynamic key. Before the fix sui evaluated the whole
    // attrpath eagerly at construction, forcing `e` — which, in the NixOS
    // module-system fixpoint, read `config.<x>` while `config` was mid-force
    // (the `homes.${cfg.userName}` → `homes.null` divergence). `u` binds the
    // dynamic key; reading `.other` proves `${u}` is not forced during
    // construction of the sibling.
    let dyn_key_entry = |head: &str, key_expr: NixValue, value: NixValue| {
        AttrSetEntry::KeyValue {
            key: gen_nix::ast::AttrPath(vec![
                gen_nix::ast::AttrKey::Ident(head.to_string()),
                gen_nix::ast::AttrKey::Interp(key_expr),
            ]),
            value,
        }
    };
    rows.push(row(
        "dynamic inner attrpath key — sibling read stays lazy",
        let_(
            vec![
                ("u", str_("bob")),
                (
                    "s",
                    attrs(vec![
                        dyn_key_entry("homes", path(&["u"]), NixValue::Int(7)),
                        entry("other", NixValue::Int(9)),
                    ]),
                ),
            ],
            path(&["s", "other"]),
        ),
        RowExpect::Match,
    ));
    // Demanding the head DOES resolve the deferred dynamic key: selecting
    // `s.homes.bob` returns 7 in both engines.
    rows.push(row(
        "dynamic inner attrpath key — head demand resolves key",
        let_(
            vec![
                ("u", str_("bob")),
                (
                    "s",
                    attrs(vec![dyn_key_entry(
                        "homes",
                        path(&["u"]),
                        NixValue::Int(7),
                    )]),
                ),
            ],
            NixValue::AttrPath(vec!["s".into(), "homes".into(), "bob".into()]),
        ),
        RowExpect::Match,
    ));

    // ── M2.6 ROOT #2: multi-level dynamic-tail attrpath stays lazy PER LEVEL ──
    // `config.homes.${u} = 7` desugars (CppNix) to nested attrset literals
    // `config = { homes = { ${u} = 7; }; }`, where EACH level is an
    // independent lazy thunk. Forcing `.config` to WHNF must yield
    // `{ homes = <thunk> }` WITHOUT forcing the `${u}` key one level deeper.
    // The ROOT #1 fix deferred a *single* dynamic tail level; the ROOT #2 fix
    // makes `build_tail_attrs_now` resolve ONE level and re-defer the rest, so
    // the over-force (sui forcing `${u}` while only `.config` — e.g. its
    // `._type` — was demanded) is gone. This is the exact shape of the NixOS
    // module-system divergence (`config.homes.${config.pleme.userName}`): the
    // definition-collection `pushDownProperties m.config` reads `m.config`'s
    // WHNF, which must not force the inner dynamic key.
    let dyn_key_entry3 = |h0: &str, h1: &str, key_expr: NixValue, value: NixValue| {
        AttrSetEntry::KeyValue {
            key: gen_nix::ast::AttrPath(vec![
                gen_nix::ast::AttrKey::Ident(h0.to_string()),
                gen_nix::ast::AttrKey::Ident(h1.to_string()),
                gen_nix::ast::AttrKey::Interp(key_expr),
            ]),
            value,
        }
    };
    // Reading the MIDDLE level's key structure (`attrNames s.config`) must not
    // force the `${u}` key: both engines yield `[ "homes" ]`.
    rows.push(row(
        "multi-level dynamic-tail attrpath — middle-level read stays lazy",
        let_(
            vec![
                ("u", str_("bob")),
                (
                    "s",
                    attrs(vec![dyn_key_entry3(
                        "config",
                        "homes",
                        path(&["u"]),
                        NixValue::Int(7),
                    )]),
                ),
            ],
            apply(path(&["builtins", "attrNames"]), vec![path(&["s", "config"])]),
        ),
        RowExpect::Match,
    ));
    // Reading a SIBLING under the same head (`s.config.other`) must not force
    // the deeper `${u}` key: both engines yield `9`.
    rows.push(row(
        "multi-level dynamic-tail attrpath — sibling under head stays lazy",
        let_(
            vec![
                ("u", str_("bob")),
                (
                    "s",
                    attrs(vec![
                        dyn_key_entry3("config", "homes", path(&["u"]), NixValue::Int(7)),
                        AttrSetEntry::KeyValue {
                            key: gen_nix::ast::AttrPath(vec![
                                gen_nix::ast::AttrKey::Ident("config".to_string()),
                                gen_nix::ast::AttrKey::Ident("other".to_string()),
                            ]),
                            value: NixValue::Int(9),
                        },
                    ]),
                ),
            ],
            NixValue::AttrPath(vec!["s".into(), "config".into(), "other".into()]),
        ),
        RowExpect::Match,
    ));
    // Demanding the leaf through both levels (`s.config.homes.bob`) DOES resolve
    // the deferred dynamic key: both engines yield `7`.
    rows.push(row(
        "multi-level dynamic-tail attrpath — leaf demand resolves key",
        let_(
            vec![
                ("u", str_("bob")),
                (
                    "s",
                    attrs(vec![dyn_key_entry3(
                        "config",
                        "homes",
                        path(&["u"]),
                        NixValue::Int(7),
                    )]),
                ),
            ],
            NixValue::AttrPath(vec![
                "s".into(),
                "config".into(),
                "homes".into(),
                "bob".into(),
            ]),
        ),
        RowExpect::Match,
    ));

    // ── M2.6 ROOT #3: dynamic tail key under a COLLIDING head stays lazy ──
    // Two bindings share the head `sd`: a static `sd.services.x` AND a
    // dynamic `sd.tmpfiles.${k}.d`. The ROOT #1/#2 deferral bailed when the
    // head already existed (collision) and the eager path forced `${k}` at
    // construction — the NixOS-module over-force (osquery's
    // `systemd.services.… = …` then
    // `systemd.tmpfiles.settings."10-osquery".${dirname …}.d`), which reads
    // `config.<x>` mid-fixpoint → the empty-Promise partial. Fixed by
    // `merge_deferred_dynamic_tail`: splice a deferred thunk under the
    // existing head. Reading the STATIC sibling (`sd.services.x`) must not
    // force `${k}` — both engines yield `1`.
    let dyn_tail_under_head = |h0: &str, h1: &str, key_expr: NixValue, leaf: &str, value: NixValue| {
        AttrSetEntry::KeyValue {
            key: gen_nix::ast::AttrPath(vec![
                gen_nix::ast::AttrKey::Ident(h0.to_string()),
                gen_nix::ast::AttrKey::Ident(h1.to_string()),
                gen_nix::ast::AttrKey::Interp(key_expr),
                gen_nix::ast::AttrKey::Ident(leaf.to_string()),
            ]),
            value,
        }
    };
    rows.push(row(
        "dynamic tail under colliding head — static sibling read stays lazy",
        let_(
            vec![
                ("k", str_("z")),
                (
                    "s",
                    attrs(vec![
                        dotted_entry("sd.services.x", NixValue::Int(1)),
                        dyn_tail_under_head("sd", "tmpfiles", path(&["k"]), "d", NixValue::Int(2)),
                    ]),
                ),
            ],
            NixValue::AttrPath(vec!["s".into(), "sd".into(), "services".into(), "x".into()]),
        ),
        RowExpect::Match,
    ));
    // Demanding the dynamic branch (`s.sd.tmpfiles.z.d`) DOES resolve the
    // deferred key AND the static sibling survives the merge — both engines
    // yield `2` here (and the sibling would still yield `1`).
    rows.push(row(
        "dynamic tail under colliding head — dynamic branch resolves",
        let_(
            vec![
                ("k", str_("z")),
                (
                    "s",
                    attrs(vec![
                        dotted_entry("sd.services.x", NixValue::Int(1)),
                        dyn_tail_under_head("sd", "tmpfiles", path(&["k"]), "d", NixValue::Int(2)),
                    ]),
                ),
            ],
            NixValue::AttrPath(vec![
                "s".into(), "sd".into(), "tmpfiles".into(), "z".into(), "d".into(),
            ]),
        ),
        RowExpect::Match,
    ));

    // ── M2.6 ROOT #4a: `with` namespace stays LAZY ──────────────────────
    // `with X; body` must NOT force X to compute the body's WHNF/keys —
    // cppnix forces the namespace ONLY on a bare-ident lookup fallthrough.
    // sui used to EVALUATE the namespace at `with`-entry, which for nixpkgs'
    // `config = mkIf … (with config.services.X; { … })` module shape forced
    // `config.services.X` mid-fixpoint during collection → the empty-Promise
    // partial → `concatLists null`.  `attrNames (with (throw "X"); {a=1;b=2;})`
    // must be `[ "a" "b" ]` (the throw never fires).
    rows.push(row(
        "with-namespace laziness — body WHNF does not force the namespace",
        apply(
            path(&["builtins", "attrNames"]),
            vec![NixValue::Raw(
                "(with (throw \"WITH-FORCED\"); { a = 1; b = 2; })".to_string(),
            )],
        ),
        RowExpect::Match,
    ));

    // ── M2.6 ROOT #4b: depth-≥2 dotted full-set leaf deep-merges ────────
    // `o.a = { x = 1; }` inserts `o = { a = <thunk {x=1}> }` (the full-set
    // leaf goes through `maybe_thunk`); a deeper sibling `o.a.y = 2` must
    // deep-merge into it, not overwrite.  sui's `merge_nested_insert`
    // dropped the earlier leaf on a Thunk-vs-Attrs collision → nixpkgs'
    // `options.hardware.alsa = { … }` + `options.hardware.alsa.enablePersistence
    // = …` merged to only `{enablePersistence}`.  Both orderings must yield
    // `[ "x" "y" ]`.
    rows.push(row(
        "dotted full-set leaf deep-merges with a deeper sibling (forward)",
        apply(
            path(&["builtins", "attrNames"]),
            vec![NixValue::Raw(
                "({ o.a = { x = 1; }; o.a.y = 2; }.o.a)".to_string(),
            )],
        ),
        RowExpect::Match,
    ));
    rows.push(row(
        "dotted full-set leaf deep-merges with a deeper sibling (reverse)",
        apply(
            path(&["builtins", "attrNames"]),
            vec![NixValue::Raw(
                "({ o.a.y = 2; o.a = { x = 1; }; }.o.a)".to_string(),
            )],
        ),
        RowExpect::Match,
    ));

    rows
}
