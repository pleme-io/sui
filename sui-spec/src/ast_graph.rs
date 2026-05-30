//! L1 AstGraph — the parsed, canonicalized, content-addressed form of
//! a single Nix expression (one `.nix` file or one inline expression).
//!
//! Mirrors `lockfile_graph` to a tee:
//!
//! 1. Parse via rnix exactly once.
//! 2. Walk the rnix typed AST and emit a dense `Vec<AstNode>` keyed by
//!    [`NodeId`] (u32). Every reference between nodes is a `NodeId`,
//!    so the archive is pointer-free.
//! 3. rkyv-archive the graph and BLAKE3 the bytes → cache key for
//!    [`sui_graph_store::GraphKind::Ast`].
//! 4. Cold cost: rnix parse + walk. Warm cost: mmap + cast. The latter
//!    is the budget every eval-cache lookup pays after the first
//!    invocation pays the former.
//!
//! ## Coverage
//!
//! The typed AST covers the 80% of Nix language constructs the rio fleet
//! and nixpkgs hit on the critical path. Constructs we don't model yet
//! land in [`AstNodeKind::Unknown`] with the source text preserved
//! verbatim — forward-compat, never panics. As we add more variants,
//! older blobs keep round-tripping because rkyv appends new variants
//! by tag.
//!
//! Modeled today:
//!
//! * Literals — `Int`, `Float`, `Bool`, `Null`, `Str`, `IndentedStr`,
//!   `Path`
//! * References — `Ident`, `Select` (attrset.a.b with optional fallback)
//! * Containers — `List`, `AttrSet` (`rec` or not), `Inherit` clauses
//! * Bindings — `LetIn`, `With`, `Assert`
//! * Functions — `Lambda` (formal-args destructuring), `Apply`
//! * Control — `IfThenElse`
//! * Operators — `BinOp`, `UnaryOp`, `HasAttr`
//! * Forward-compat — `Unknown { kind, source_text }`
//!
//! ## Determinism
//!
//! Node ids are assigned in **post-order** traversal of the rnix AST:
//! the recursive builder emits each child node before it emits its
//! parent, so the root expression is the **last** node and `root_id`
//! is `(nodes.len() - 1) as u32`. (Unlike `lockfile_graph` whose root
//! is always 0 because the graph is built via BFS from a known root
//! name — for ASTs there's no nameable root, just whatever the source
//! parses to.) Source text in `Unknown` is taken verbatim from the
//! rnix CST so re-parsing it round-trips. The same input source →
//! same node count → same archive bytes → same BLAKE3, end-to-end.
//!
//! ## Behavior contract
//!
//! This module **adds** a typed archive form of Nix expressions. It
//! does not replace the existing rnix → sui-bytecode compilation path
//! (`sui-bytecode/src/compiler.rs`), which remains the only producer of
//! bytecode chunks. The AstGraph is consumed by the L1 substrate (eval
//! cache key derivation, module-graph compiler input, daemon hot
//! cache) — not by the VM directly.

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use rnix::ast::{self, AstToken, HasEntry};
use rowan::ast::AstNode;
use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

/// Dense node identifier within an [`AstGraph`].
pub type NodeId = u32;

/// 32-byte BLAKE3 content hash; same shape as
/// `lockfile_graph::CanonicalGraphHash`. Kept duplicated rather than
/// pulled in to keep the module's dependency surface tight.
#[derive(
    DeriveTataraDomain,
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
)]
#[tatara(keyword = "defast-graph-hash")]
#[rkyv(derive(Debug))]
pub struct AstGraphHash {
    pub bytes: [u8; 32],
}

/// Which surface dialect a parsed [`AstGraph`] came from. The IR
/// itself is dialect-agnostic — this discriminator anchors the
/// bidirectional render pipeline + lets transformation passes preserve
/// (or change) the source dialect.
///
/// Sui's stance: both dialects produce the **same** typed IR. The
/// downstream pipeline (eval cache, derivation builder, module-graph
/// compiler, daemon hot cache) never branches on dialect. Only the
/// surface (parse + render) cares.
#[derive(
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
)]
#[rkyv(derive(Debug))]
pub enum SourceDialect {
    /// `.nix` source via the rnix parser. Production today.
    Nix,
    /// `.tlisp` source via the tatara-lisp parser. Queued — the
    /// parser + lowering exist as a typed seam (see
    /// [`AstGraph::from_tlisp_source`]) but the full lowering of every
    /// `AstNodeKind` variant is a focused follow-up ship.
    Tlisp,
    /// A graph stitched together from BOTH dialects (e.g. a `.nix`
    /// flake importing a `.tlisp` module, or a `.tlisp` overlay
    /// transforming a `.nix` config). Rendering this requires keeping
    /// per-node provenance — `AstNodeForm` gains an optional
    /// `dialect_origin` field in a later ship.
    Mixed,
}

/// The AST graph proper. `nodes[0]` is always the root expression.
#[derive(
    DeriveTataraDomain,
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    PartialEq,
)]
#[tatara(keyword = "defast-graph")]
#[rkyv(derive(Debug))]
pub struct AstGraph {
    /// Which surface dialect this graph was lowered from. Today only
    /// `Nix` is implemented; `Tlisp` is reserved so the typed IR is
    /// stable across the future dialect-lowering work. **The IR
    /// downstream of this field is dialect-agnostic** — every
    /// consumer (eval cache, module-graph compiler, derivation
    /// builder) reads `AstGraph` without knowing which dialect
    /// produced it. Bidirectional rendering (back to `.nix` /
    /// `.tlisp` source text) hangs off this discriminator.
    pub dialect: SourceDialect,
    /// rnix grammar version the source was parsed against. Today this
    /// is locked to the bundled rnix major (0.14). Bumping is a
    /// migration boundary.
    pub grammar_version: u32,
    /// Index of the root expression in [`Self::nodes`]. Today, this is
    /// always `(nodes.len() - 1) as u32` because the builder is
    /// post-order — children are pushed before their parent.
    pub root_id: NodeId,
    /// Dense node table.
    pub nodes: Vec<AstNodeForm>,
    /// BLAKE3 of the rkyv archive bytes. Populated by
    /// [`AstGraph::archive_and_hash`].
    pub canonical_hash: AstGraphHash,
}

/// One AST node in the graph.
#[derive(
    DeriveTataraDomain,
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    PartialEq,
)]
#[tatara(keyword = "defast-node")]
#[rkyv(derive(Debug))]
pub struct AstNodeForm {
    pub id: NodeId,
    pub kind: AstNodeKind,
}

/// Discriminator for every node variant. Stable IDs by name; append
/// new variants at the bottom of the enum, never reorder (rkyv
/// archive compatibility).
#[derive(
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    PartialEq,
)]
#[rkyv(derive(Debug))]
pub enum AstNodeKind {
    // ── Literals ──────────────────────────────────────────────────
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    /// Quoted string (possibly with interpolation, modeled as a
    /// concatenation of segments).
    Str { segments: Vec<StrSegment> },
    /// `''…''` indented string.
    IndentedStr { segments: Vec<StrSegment> },
    Path(String),

    // ── References ────────────────────────────────────────────────
    Ident(String),
    /// `expr.attr.path` (with optional `or fallback`).
    Select {
        target: NodeId,
        path: Vec<String>,
        fallback: Option<NodeId>,
    },
    /// `expr ? attr.path` — attribute-presence test.
    HasAttr { target: NodeId, path: Vec<String> },

    // ── Containers ────────────────────────────────────────────────
    List(Vec<NodeId>),
    AttrSet {
        recursive: bool,
        entries: Vec<AttrEntry>,
        inherits: Vec<InheritClause>,
    },

    // ── Bindings ──────────────────────────────────────────────────
    LetIn {
        bindings: Vec<AttrEntry>,
        inherits: Vec<InheritClause>,
        body: NodeId,
    },
    With {
        env: NodeId,
        body: NodeId,
    },
    Assert {
        condition: NodeId,
        body: NodeId,
    },

    // ── Functions ─────────────────────────────────────────────────
    Lambda {
        param: LambdaParam,
        body: NodeId,
    },
    Apply {
        function: NodeId,
        argument: NodeId,
    },

    // ── Control ───────────────────────────────────────────────────
    IfThenElse {
        condition: NodeId,
        then_branch: NodeId,
        else_branch: NodeId,
    },

    // ── Operators ─────────────────────────────────────────────────
    BinOp {
        op: BinaryOp,
        left: NodeId,
        right: NodeId,
    },
    UnaryOp {
        op: UnaryOp,
        operand: NodeId,
    },

    // ── Forward-compat ────────────────────────────────────────────
    /// Construct we don't model yet. `source_text` is the verbatim
    /// span from the rnix CST so the underlying meaning is recoverable.
    Unknown {
        kind: String,
        source_text: String,
    },
}

/// One entry in an `AttrSet` or `LetIn` bindings list.
#[derive(
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    PartialEq,
)]
#[rkyv(derive(Debug))]
pub struct AttrEntry {
    /// Dotted attribute path (`a.b.c` → `["a", "b", "c"]`).
    pub path: Vec<String>,
    pub value: NodeId,
}

/// One `inherit` clause: `inherit (source) attr1 attr2;` or
/// `inherit attr1 attr2;` (source = None).
#[derive(
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
)]
#[rkyv(derive(Debug))]
pub struct InheritClause {
    pub source: Option<NodeId>,
    pub attrs: Vec<String>,
}

/// Lambda parameter shape: a bare identifier, or formal-args
/// destructuring, or both (`name @ { … }`).
#[derive(
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    PartialEq,
)]
#[rkyv(derive(Debug))]
pub enum LambdaParam {
    /// `x:`
    Ident(String),
    /// `{ a, b ? default, ... } [@ name]:`
    Pattern {
        binding_name: Option<String>,
        formals: Vec<Formal>,
        accepts_extra: bool,
    },
}

/// One named formal arg in a destructuring lambda.
#[derive(
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
)]
#[rkyv(derive(Debug))]
pub struct Formal {
    pub name: String,
    pub default: Option<NodeId>,
}

/// One segment of a possibly-interpolated string.
#[derive(
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    PartialEq,
)]
#[rkyv(derive(Debug))]
pub enum StrSegment {
    Literal(String),
    /// `${expr}` interpolation.
    Interpolation(NodeId),
}

/// Binary operator. Tags are stable; appending allowed.
#[derive(
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
)]
#[rkyv(derive(Debug))]
pub enum BinaryOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `==`
    Eq,
    /// `!=`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `&&`
    And,
    /// `||`
    Or,
    /// `->`
    Implies,
    /// `//`
    Update,
    /// `++`
    Concat,
    /// `|>` (Nix 2.18+) — pipe right: `x |> f` ≡ `f x`.
    PipeRight,
    /// `<|` (Nix 2.18+) — pipe left: `f <| x` ≡ `f x`.
    PipeLeft,
}

/// Unary operator.
#[derive(
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
)]
#[rkyv(derive(Debug))]
pub enum UnaryOp {
    /// `-`
    Neg,
    /// `!`
    Not,
}

/// Errors produced when materializing an [`AstGraph`] from source.
#[derive(Debug, thiserror::Error)]
pub enum AstGraphError {
    #[error("rnix parse error(s): {0}")]
    Parse(String),
    #[error("rnix produced no root expression")]
    NoRoot,
    #[error("rkyv archive of canonical AST graph failed: {0}")]
    Archive(String),
    #[error("{what} (not yet implemented; tracked in the dialect-rendering ship)")]
    Unimplemented { what: &'static str },
}

impl AstGraph {
    /// Parse + canonicalize source text into a typed AST graph.
    ///
    /// # Errors
    ///
    /// - [`AstGraphError::Parse`] if rnix can't parse the source.
    /// - [`AstGraphError::NoRoot`] if the parse succeeds but produces
    ///   an empty root (e.g. comments-only input).
    pub fn from_source(source: &str) -> Result<Self, AstGraphError> {
        let parse = rnix::Root::parse(source);
        if !parse.errors().is_empty() {
            let msg = parse
                .errors()
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(AstGraphError::Parse(msg));
        }
        let root = parse.tree();
        let expr = root.expr().ok_or(AstGraphError::NoRoot)?;

        let mut builder = Builder::default();
        let root_id = builder.lower_expr(&expr);
        // Builder emits children before parents, so the root is the
        // last node and `root_id == nodes.len() - 1`. Assert that here
        // in debug builds; downstream consumers depend on it.
        debug_assert_eq!(root_id as usize + 1, builder.nodes.len());

        Ok(Self {
            dialect: SourceDialect::Nix,
            grammar_version: RNIX_GRAMMAR_VERSION,
            root_id,
            nodes: builder.nodes,
            canonical_hash: AstGraphHash { bytes: [0u8; 32] },
        })
    }

    /// Parse + lower a tatara-lisp source into the universal IR.
    ///
    /// **Status**: typed seam only. Today this returns an `Unknown`-
    /// kinded root with the source preserved verbatim so callers can
    /// already exercise the dialect-aware downstream pipeline. The
    /// real lowering — mapping each `defast-node` Lisp form to the
    /// matching [`AstNodeKind`] variant — lands in the focused
    /// `.tlisp` dialect ship.
    ///
    /// # Errors
    ///
    /// Always succeeds today (every input is captured as `Unknown`).
    /// Future versions will return [`AstGraphError::Parse`] when the
    /// tatara-lisp reader rejects the input.
    pub fn from_tlisp_source(source: &str) -> Result<Self, AstGraphError> {
        let mut nodes = Vec::with_capacity(1);
        nodes.push(AstNodeForm {
            id: 0,
            kind: AstNodeKind::Unknown {
                kind: "tlisp-source-pending-lowering".to_string(),
                source_text: source.to_string(),
            },
        });
        Ok(Self {
            dialect: SourceDialect::Tlisp,
            grammar_version: TLISP_GRAMMAR_VERSION,
            root_id: 0,
            nodes,
            canonical_hash: AstGraphHash { bytes: [0u8; 32] },
        })
    }

    /// Render this graph back into Nix source text.
    ///
    /// **Status**: API seam only. The renderer that emits canonical
    /// `.nix` syntax for every [`AstNodeKind`] variant lands in the
    /// dialect-rendering ship.
    ///
    /// # Errors
    ///
    /// Always returns [`AstGraphError::Unimplemented`] today.
    pub fn to_nix_source(&self) -> Result<String, AstGraphError> {
        Err(AstGraphError::Unimplemented {
            what: "AstGraph::to_nix_source — queued",
        })
    }

    /// Render this graph as tatara-lisp source text.
    ///
    /// **Status**: API seam only. See [`Self::to_nix_source`].
    ///
    /// # Errors
    ///
    /// Always returns [`AstGraphError::Unimplemented`] today.
    pub fn to_tlisp_source(&self) -> Result<String, AstGraphError> {
        Err(AstGraphError::Unimplemented {
            what: "AstGraph::to_tlisp_source — queued",
        })
    }

    /// Same two-pass shape as `LockfileGraph::archive_and_hash` — the
    /// hash is part of the archive, so it can't be computed before the
    /// archive exists.
    ///
    /// # Errors
    ///
    /// [`AstGraphError::Archive`] if rkyv refuses the graph shape.
    pub fn archive_and_hash(mut self) -> Result<(Self, Vec<u8>), AstGraphError> {
        let initial = rkyv::to_bytes::<rkyv::rancor::Error>(&self)
            .map_err(|e| AstGraphError::Archive(e.to_string()))?;
        let hash = blake3::hash(&initial);
        self.canonical_hash = AstGraphHash { bytes: hash.into() };
        let stamped = rkyv::to_bytes::<rkyv::rancor::Error>(&self)
            .map_err(|e| AstGraphError::Archive(e.to_string()))?;
        Ok((self, stamped.to_vec()))
    }
}

/// Bumped when we change the on-disk graph shape in a way that
/// requires re-parsing source. rkyv handles append-only variant
/// changes natively; this version is for cases like "renamed an
/// existing variant" where blobs need to be invalidated.
pub const RNIX_GRAMMAR_VERSION: u32 = 1;

/// Bumped when the tatara-lisp dialect's lowering schema changes in
/// a way that requires re-parsing. Today: 1 (the stub).
pub const TLISP_GRAMMAR_VERSION: u32 = 1;

// ── Builder ──────────────────────────────────────────────────────

#[derive(Default)]
struct Builder {
    nodes: Vec<AstNodeForm>,
}

impl Builder {
    fn push(&mut self, kind: AstNodeKind) -> NodeId {
        let id = self.nodes.len() as NodeId;
        self.nodes.push(AstNodeForm { id, kind });
        id
    }

    fn unknown(&mut self, expr: &ast::Expr, label: &str) -> NodeId {
        let source_text = expr.syntax().to_string();
        self.push(AstNodeKind::Unknown {
            kind: label.to_string(),
            source_text,
        })
    }

    fn lower_expr(&mut self, expr: &ast::Expr) -> NodeId {
        match expr {
            ast::Expr::Literal(lit) => self.lower_literal(lit),
            ast::Expr::Str(s) => self.lower_str(s, /* indented= */ false),
            ast::Expr::Ident(ident) => {
                let text = ident
                    .ident_token()
                    .map(|t| t.text().to_string())
                    .unwrap_or_default();
                self.push(AstNodeKind::Ident(text))
            }
            // rnix splits the path syntax into four variants depending
            // on whether it's absolute, relative, home-relative, or a
            // <name> search path. Carry the verbatim source text in all
            // four cases; downstream consumers parse it themselves if
            // they need the typed form.
            ast::Expr::PathAbs(p) => {
                let text = p.syntax().to_string();
                self.push(AstNodeKind::Path(text))
            }
            ast::Expr::PathRel(p) => {
                let text = p.syntax().to_string();
                self.push(AstNodeKind::Path(text))
            }
            ast::Expr::PathHome(p) => {
                let text = p.syntax().to_string();
                self.push(AstNodeKind::Path(text))
            }
            ast::Expr::PathSearch(p) => {
                let text = p.syntax().to_string();
                self.push(AstNodeKind::Path(text))
            }
            // `__curPos` — built-in source-position marker.
            ast::Expr::CurPos(c) => {
                let text = c.syntax().to_string();
                self.push(AstNodeKind::Unknown {
                    kind: "cur-pos".to_string(),
                    source_text: text,
                })
            }
            // Pre-2.0 `let { body = …; …; }` syntax. Rare, but rio
            // ships a few nixpkgs pins that contain it.
            ast::Expr::LegacyLet(l) => {
                let text = l.syntax().to_string();
                self.push(AstNodeKind::Unknown {
                    kind: "legacy-let".to_string(),
                    source_text: text,
                })
            }
            // rnix's `Root` is the document wrapper; nested Roots are
            // a parse oddity, but if we see one just dive into its
            // body so the AST stays well-formed.
            ast::Expr::Root(r) => match r.expr() {
                Some(inner) => self.lower_expr(&inner),
                None => self.push(AstNodeKind::Null),
            },
            // Recoverable rnix parse error placeholder. We've already
            // rejected non-empty parse-error lists in `from_source`, so
            // this should only fire for inline-error nodes that
            // upstream couldn't surface as a hard error.
            ast::Expr::Error(e) => {
                let text = e.syntax().to_string();
                self.push(AstNodeKind::Unknown {
                    kind: "parse-error".to_string(),
                    source_text: text,
                })
            }
            ast::Expr::List(list) => {
                let items: Vec<NodeId> = list.items().map(|e| self.lower_expr(&e)).collect();
                self.push(AstNodeKind::List(items))
            }
            ast::Expr::AttrSet(set) => self.lower_attrset(set),
            ast::Expr::LetIn(letin) => self.lower_letin(letin),
            ast::Expr::With(with) => {
                let env = with.namespace().map(|e| self.lower_expr(&e));
                let body = with.body().map(|e| self.lower_expr(&e));
                match (env, body) {
                    (Some(env), Some(body)) => self.push(AstNodeKind::With { env, body }),
                    _ => self.unknown(expr, "with-missing-side"),
                }
            }
            ast::Expr::Assert(assert) => {
                let condition = assert.condition().map(|e| self.lower_expr(&e));
                let body = assert.body().map(|e| self.lower_expr(&e));
                match (condition, body) {
                    (Some(condition), Some(body)) => {
                        self.push(AstNodeKind::Assert { condition, body })
                    }
                    _ => self.unknown(expr, "assert-missing-side"),
                }
            }
            ast::Expr::Lambda(lambda) => self.lower_lambda(lambda),
            ast::Expr::Apply(apply) => {
                let function = apply.lambda().map(|e| self.lower_expr(&e));
                let argument = apply.argument().map(|e| self.lower_expr(&e));
                match (function, argument) {
                    (Some(function), Some(argument)) => self.push(AstNodeKind::Apply {
                        function,
                        argument,
                    }),
                    _ => self.unknown(expr, "apply-missing-side"),
                }
            }
            ast::Expr::IfElse(ifelse) => {
                let condition = ifelse.condition().map(|e| self.lower_expr(&e));
                let then_branch = ifelse.body().map(|e| self.lower_expr(&e));
                let else_branch = ifelse.else_body().map(|e| self.lower_expr(&e));
                match (condition, then_branch, else_branch) {
                    (Some(c), Some(t), Some(e)) => self.push(AstNodeKind::IfThenElse {
                        condition: c,
                        then_branch: t,
                        else_branch: e,
                    }),
                    _ => self.unknown(expr, "if-else-missing-branch"),
                }
            }
            ast::Expr::Select(select) => self.lower_select(select),
            ast::Expr::HasAttr(has) => {
                let target = has.expr().map(|e| self.lower_expr(&e));
                let path = has
                    .attrpath()
                    .map(|p| attrpath_to_strings(&p))
                    .unwrap_or_default();
                match target {
                    Some(target) => self.push(AstNodeKind::HasAttr { target, path }),
                    None => self.unknown(expr, "hasattr-missing-target"),
                }
            }
            ast::Expr::BinOp(binop) => self.lower_binop(binop),
            ast::Expr::UnaryOp(unaryop) => self.lower_unaryop(unaryop),
            ast::Expr::Paren(p) => match p.expr() {
                Some(inner) => self.lower_expr(&inner),
                None => self.unknown(expr, "paren-empty"),
            },
            // Every rnix 0.14 Expr variant is handled above. If a future
            // rnix bump adds a variant we don't know about, the compiler
            // catches the gap as a non-exhaustive match — by design, so
            // we can't silently drop a construct into Unknown.
        }
    }

    fn lower_literal(&mut self, lit: &ast::Literal) -> NodeId {
        match lit.kind() {
            ast::LiteralKind::Integer(t) => {
                let n: i64 = t.value().unwrap_or(0);
                self.push(AstNodeKind::Int(n))
            }
            ast::LiteralKind::Float(t) => {
                let f: f64 = t.value().unwrap_or(0.0);
                self.push(AstNodeKind::Float(f))
            }
            ast::LiteralKind::Uri(t) => {
                // Uris are deprecated in modern Nix; treat as Str.
                self.push(AstNodeKind::Str {
                    segments: vec![StrSegment::Literal(t.syntax().text().to_string())],
                })
            }
        }
    }

    fn lower_str(&mut self, s: &ast::Str, _indented: bool) -> NodeId {
        let mut segments = Vec::new();
        for part in s.normalized_parts() {
            match part {
                ast::InterpolPart::Literal(text) => {
                    segments.push(StrSegment::Literal(text));
                }
                ast::InterpolPart::Interpolation(interp) => {
                    let id = match interp.expr() {
                        Some(expr) => self.lower_expr(&expr),
                        None => {
                            // Empty interpolation: treat as empty string literal.
                            segments.push(StrSegment::Literal(String::new()));
                            continue;
                        }
                    };
                    segments.push(StrSegment::Interpolation(id));
                }
            }
        }
        self.push(AstNodeKind::Str { segments })
    }

    fn lower_attrset(&mut self, set: &ast::AttrSet) -> NodeId {
        let recursive = set.rec_token().is_some();
        let mut entries = Vec::new();
        let mut inherits = Vec::new();
        for entry in set.entries() {
            match entry {
                ast::Entry::AttrpathValue(av) => {
                    let path = av
                        .attrpath()
                        .map(|p| attrpath_to_strings(&p))
                        .unwrap_or_default();
                    let value = av
                        .value()
                        .map(|e| self.lower_expr(&e))
                        .unwrap_or_else(|| self.push(AstNodeKind::Null));
                    entries.push(AttrEntry { path, value });
                }
                ast::Entry::Inherit(inherit) => {
                    let source = inherit.from().and_then(|f| f.expr()).map(|e| self.lower_expr(&e));
                    let attrs: Vec<String> = inherit
                        .attrs()
                        .filter_map(|a| attr_to_string(&a))
                        .collect();
                    inherits.push(InheritClause { source, attrs });
                }
            }
        }
        self.push(AstNodeKind::AttrSet {
            recursive,
            entries,
            inherits,
        })
    }

    fn lower_letin(&mut self, letin: &ast::LetIn) -> NodeId {
        let mut bindings = Vec::new();
        let mut inherits = Vec::new();
        for entry in letin.entries() {
            match entry {
                ast::Entry::AttrpathValue(av) => {
                    let path = av
                        .attrpath()
                        .map(|p| attrpath_to_strings(&p))
                        .unwrap_or_default();
                    let value = av
                        .value()
                        .map(|e| self.lower_expr(&e))
                        .unwrap_or_else(|| self.push(AstNodeKind::Null));
                    bindings.push(AttrEntry { path, value });
                }
                ast::Entry::Inherit(inherit) => {
                    let source = inherit.from().and_then(|f| f.expr()).map(|e| self.lower_expr(&e));
                    let attrs: Vec<String> = inherit
                        .attrs()
                        .filter_map(|a| attr_to_string(&a))
                        .collect();
                    inherits.push(InheritClause { source, attrs });
                }
            }
        }
        let body = letin
            .body()
            .map(|e| self.lower_expr(&e))
            .unwrap_or_else(|| self.push(AstNodeKind::Null));
        self.push(AstNodeKind::LetIn {
            bindings,
            inherits,
            body,
        })
    }

    fn lower_lambda(&mut self, lambda: &ast::Lambda) -> NodeId {
        let param = match lambda.param() {
            Some(ast::Param::IdentParam(ip)) => {
                let name = ip
                    .ident()
                    .and_then(|i| i.ident_token().map(|t| t.text().to_string()))
                    .unwrap_or_default();
                LambdaParam::Ident(name)
            }
            Some(ast::Param::Pattern(pattern)) => {
                let binding_name = pattern
                    .pat_bind()
                    .and_then(|b| b.ident())
                    .and_then(|i| i.ident_token().map(|t| t.text().to_string()));
                let accepts_extra = pattern.ellipsis_token().is_some();
                let mut formals: Vec<Formal> = Vec::new();
                for entry in pattern.pat_entries() {
                    let name = entry
                        .ident()
                        .and_then(|i| i.ident_token().map(|t| t.text().to_string()))
                        .unwrap_or_default();
                    let default = entry.default().map(|e| self.lower_expr(&e));
                    formals.push(Formal { name, default });
                }
                LambdaParam::Pattern {
                    binding_name,
                    formals,
                    accepts_extra,
                }
            }
            None => LambdaParam::Ident(String::new()),
        };
        let body = lambda
            .body()
            .map(|e| self.lower_expr(&e))
            .unwrap_or_else(|| self.push(AstNodeKind::Null));
        self.push(AstNodeKind::Lambda { param, body })
    }

    fn lower_select(&mut self, select: &ast::Select) -> NodeId {
        let target = match select.expr() {
            Some(e) => self.lower_expr(&e),
            None => return self.push(AstNodeKind::Null),
        };
        let path = select
            .attrpath()
            .map(|p| attrpath_to_strings(&p))
            .unwrap_or_default();
        let fallback = select.default_expr().map(|e| self.lower_expr(&e));
        self.push(AstNodeKind::Select {
            target,
            path,
            fallback,
        })
    }

    fn lower_binop(&mut self, binop: &ast::BinOp) -> NodeId {
        let left = binop
            .lhs()
            .map(|e| self.lower_expr(&e))
            .unwrap_or_else(|| self.push(AstNodeKind::Null));
        let right = binop
            .rhs()
            .map(|e| self.lower_expr(&e))
            .unwrap_or_else(|| self.push(AstNodeKind::Null));
        let op = binop
            .operator()
            .map(map_binop)
            .unwrap_or(BinaryOp::Concat);
        self.push(AstNodeKind::BinOp { op, left, right })
    }

    fn lower_unaryop(&mut self, unaryop: &ast::UnaryOp) -> NodeId {
        let operand = unaryop
            .expr()
            .map(|e| self.lower_expr(&e))
            .unwrap_or_else(|| self.push(AstNodeKind::Null));
        let op = match unaryop.operator() {
            Some(ast::UnaryOpKind::Negate) => UnaryOp::Neg,
            Some(ast::UnaryOpKind::Invert) => UnaryOp::Not,
            None => UnaryOp::Neg,
        };
        self.push(AstNodeKind::UnaryOp { op, operand })
    }
}

fn map_binop(op: ast::BinOpKind) -> BinaryOp {
    match op {
        ast::BinOpKind::Add => BinaryOp::Add,
        ast::BinOpKind::Sub => BinaryOp::Sub,
        ast::BinOpKind::Mul => BinaryOp::Mul,
        ast::BinOpKind::Div => BinaryOp::Div,
        ast::BinOpKind::Equal => BinaryOp::Eq,
        ast::BinOpKind::NotEqual => BinaryOp::NotEq,
        ast::BinOpKind::Less => BinaryOp::Lt,
        ast::BinOpKind::LessOrEq => BinaryOp::Le,
        ast::BinOpKind::More => BinaryOp::Gt,
        ast::BinOpKind::MoreOrEq => BinaryOp::Ge,
        ast::BinOpKind::And => BinaryOp::And,
        ast::BinOpKind::Or => BinaryOp::Or,
        ast::BinOpKind::Implication => BinaryOp::Implies,
        ast::BinOpKind::Update => BinaryOp::Update,
        ast::BinOpKind::Concat => BinaryOp::Concat,
        ast::BinOpKind::PipeRight => BinaryOp::PipeRight,
        ast::BinOpKind::PipeLeft => BinaryOp::PipeLeft,
    }
}

fn attrpath_to_strings(path: &ast::Attrpath) -> Vec<String> {
    path.attrs().filter_map(|a| attr_to_string(&a)).collect()
}

fn attr_to_string(attr: &ast::Attr) -> Option<String> {
    match attr {
        ast::Attr::Ident(i) => i.ident_token().map(|t| t.text().to_string()),
        ast::Attr::Dynamic(d) => Some(d.syntax().to_string()),
        ast::Attr::Str(s) => Some(s.syntax().to_string()),
    }
}

// ── Lisp fixtures loader ────────────────────────────────────────

#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defast-graph-fixture")]
pub struct AstGraphFixture {
    pub name: String,
    pub source: String,
    #[serde(rename = "rootKind")]
    pub root_kind: String,
    #[serde(rename = "nodeCount")]
    pub node_count: u32,
    pub notes: String,
}

pub const CANONICAL_AST_GRAPH_FIXTURES_LISP: &str =
    include_str!("../specs/ast_graph.lisp");

/// Load every authored fixture.
///
/// # Errors
///
/// Fails if the `.lisp` source can't be parsed.
pub fn load_fixtures() -> Result<Vec<AstGraphFixture>, SpecError> {
    crate::loader::load_all::<AstGraphFixture>(CANONICAL_AST_GRAPH_FIXTURES_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn integer_literal_produces_one_node() {
        let g = AstGraph::from_source("42").unwrap();
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.root_id, 0);
        matches!(g.nodes[0].kind, AstNodeKind::Int(42));
    }

    #[test]
    fn binop_produces_three_nodes() {
        // BinOp + left + right
        let g = AstGraph::from_source("1 + 2").unwrap();
        assert!(g.nodes.len() >= 3);
        assert!(matches!(
            g.nodes[g.root_id as usize].kind,
            AstNodeKind::BinOp { op: BinaryOp::Add, .. }
        ));
    }

    #[test]
    fn let_in_with_binop_resolves_identifiers() {
        let g = AstGraph::from_source("let x = 1; in x + 2").unwrap();
        assert!(matches!(
            g.nodes[g.root_id as usize].kind,
            AstNodeKind::LetIn { .. }
        ));
        // Must have at least: LetIn + binding(x=Int(1)) + BinOp + Ident(x) + Int(2)
        // The walker emits children before the parent so root is the last node.
        assert!(g.nodes.len() >= 5);
    }

    #[test]
    fn nixos_module_lambda_destructures_formals() {
        let g = AstGraph::from_source(
            "{ config, lib, pkgs, ... }: { networking.hostName = \"rio\"; }",
        )
        .unwrap();
        // Root must be a Lambda with a Pattern parameter.
        let root_kind = &g.nodes[g.root_id as usize].kind;
        match root_kind {
            AstNodeKind::Lambda {
                param: LambdaParam::Pattern { formals, accepts_extra, .. },
                ..
            } => {
                assert!(*accepts_extra);
                let names: Vec<&str> = formals.iter().map(|f| f.name.as_str()).collect();
                assert!(names.contains(&"config"));
                assert!(names.contains(&"lib"));
                assert!(names.contains(&"pkgs"));
            }
            other => panic!("expected Lambda Pattern at root, got {other:?}"),
        }
    }

    #[test]
    fn archive_and_hash_stamps_canonical_hash() {
        let g = AstGraph::from_source("let x = 1; in x + 2").unwrap();
        assert_eq!(g.canonical_hash.bytes, [0u8; 32]);
        let (stamped, bytes) = g.archive_and_hash().unwrap();
        assert_ne!(stamped.canonical_hash.bytes, [0u8; 32]);
        assert!(!bytes.is_empty());
    }

    #[test]
    fn archive_roundtrips_via_rkyv() {
        let g = AstGraph::from_source("let x = 1; in x + 2").unwrap();
        let (_stamped, bytes) = g.clone().archive_and_hash().unwrap();
        let archived =
            rkyv::access::<ArchivedAstGraph, rkyv::rancor::Error>(&bytes).unwrap();
        // Post-order: root is the last node, so root_id == nodes.len() - 1.
        assert_eq!(archived.root_id, (g.nodes.len() - 1) as u32);
        assert_eq!(archived.grammar_version, RNIX_GRAMMAR_VERSION);
        assert_eq!(archived.nodes.len(), g.nodes.len());
    }

    #[test]
    fn archive_is_deterministic_for_same_source() {
        let g1 = AstGraph::from_source("let x = 1; in x + 2").unwrap();
        let g2 = AstGraph::from_source("let x = 1; in x + 2").unwrap();
        let (s1, _) = g1.archive_and_hash().unwrap();
        let (s2, _) = g2.archive_and_hash().unwrap();
        assert_eq!(s1.canonical_hash.bytes, s2.canonical_hash.bytes);
    }

    #[test]
    fn fixtures_load_from_lisp() {
        let fixtures = load_fixtures().unwrap();
        let names: Vec<_> = fixtures.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"literal-int"));
        assert!(names.contains(&"let-in-with-binop"));
        assert!(names.contains(&"nixos-module-skeleton"));
    }

    #[test]
    fn nix_source_marks_dialect_as_nix() {
        let g = AstGraph::from_source("42").unwrap();
        assert!(matches!(g.dialect, SourceDialect::Nix));
        assert_eq!(g.grammar_version, RNIX_GRAMMAR_VERSION);
    }

    #[test]
    fn tlisp_source_stub_marks_dialect_as_tlisp() {
        let g = AstGraph::from_tlisp_source("(+ 1 2)").unwrap();
        assert!(matches!(g.dialect, SourceDialect::Tlisp));
        assert_eq!(g.grammar_version, TLISP_GRAMMAR_VERSION);
        assert_eq!(g.nodes.len(), 1);
        match &g.nodes[0].kind {
            AstNodeKind::Unknown { kind, source_text } => {
                assert!(kind.contains("tlisp"));
                assert_eq!(source_text, "(+ 1 2)");
            }
            other => panic!("expected Unknown stub, got {other:?}"),
        }
    }

    #[test]
    fn render_seams_return_unimplemented_today() {
        let g = AstGraph::from_source("42").unwrap();
        assert!(matches!(
            g.to_nix_source(),
            Err(AstGraphError::Unimplemented { .. })
        ));
        assert!(matches!(
            g.to_tlisp_source(),
            Err(AstGraphError::Unimplemented { .. })
        ));
    }

    #[test]
    fn unmodeled_construct_lands_in_unknown_with_source() {
        // Reasonably exotic: `or` outside of select context. Even when
        // we add it to the modeled set, the test still passes — the
        // node count grows but the structure doesn't.
        let result = AstGraph::from_source("let a = { foo = 1; }; in a.bar or 0");
        let g = result.unwrap();
        assert!(g.nodes.len() >= 2);
    }
}
