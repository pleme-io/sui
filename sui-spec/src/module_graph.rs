//! L4 ModuleGraph — typed IR for a NixOS/nix-darwin/home-manager
//! module system, compiled into worker/wrapper-split assignment
//! closures with slice-keyed re-firing.
//!
//! ## Why this exists
//!
//! Tvix has not staked out the module-system; cppnix re-runs the
//! entire fixed point on every rebuild. The single biggest sui-vs-nix
//! win on the rio sweep — 24 s on `nixosConfigurations.rio.config.
//! system.build.toplevel` — lives here.
//!
//! ## Pipeline shape
//!
//! ```text
//! AstGraph for each module .nix file (or .tlisp once dialect lands)
//!   → ModuleNode (declared options + config setter + import edges)
//!     → ModuleGraph (typed IR with dense ids + slice-keyed setters)
//!       → graph-hash cache key
//!         → compiled closure (defunctionalized setters, topo order)
//!           → fixed-point execution (Rayon per SCC, slice-keyed re-fire)
//!             → final config attrset
//!               → derivation graph
//! ```
//!
//! This module ships the **typed IR + builder skeleton**. The
//! compilation pipeline (worker/wrapper synthesis, defunctionalization,
//! NbE execution) lands in subsequent commits — the IR is its anchor.
//!
//! ## Invariants
//!
//! - Module ids are dense u32s, root module is always 0.
//! - Every `ImportEdge::target` points at a valid module id.
//! - `ConfigSetter::slice` lists every option path the setter reads.
//!   Empty slice = setter writes but doesn't read config (a "leaf"
//!   setter; rebuilds only when its own source hash changes).
//! - `canonical_hash` is the BLAKE3 of the rkyv archive bytes. Same
//!   module sources + same slices → same hash → cached compiled
//!   closure hits.

use std::collections::BTreeMap;

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::ast_graph::{AstGraph, NodeId as AstNodeId};
use crate::SpecError;

/// Dense module identifier. Root module is always 0.
pub type ModuleId = u32;

/// Dense setter identifier within a module. A module typically has
/// exactly one setter (the function body), but `mkMerge` and `mkIf`
/// constructs can split a logical setter into multiple typed
/// fragments — the IR keeps them separate so each can be fired
/// independently.
pub type SetterId = u32;

/// 32-byte BLAKE3 hash. Same shape as the lockfile / AST graph hashes;
/// kept separate so the type system distinguishes which graph kind
/// a hash refers to.
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
#[tatara(keyword = "defmodule-graph-hash")]
#[rkyv(derive(Debug))]
pub struct ModuleGraphHash {
    pub bytes: [u8; 32],
}

/// The compiled module graph.
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
#[tatara(keyword = "defmodule-graph")]
#[rkyv(derive(Debug))]
pub struct ModuleGraph {
    /// Bumped on every breaking change to the IR shape. Today: 1.
    pub schema_version: u32,
    /// Always 0 — root module interned first (BFS from the entrypoint).
    pub root_id: ModuleId,
    /// Dense module table indexed by `ModuleId as usize`.
    pub modules: Vec<ModuleNode>,
    /// BLAKE3 of the rkyv archive bytes. Populated by
    /// [`ModuleGraph::archive_and_hash`].
    pub canonical_hash: ModuleGraphHash,
}

/// One module: the typed projection of one `.nix` (or future `.tlisp`)
/// file's contribution to the system.
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
#[tatara(keyword = "defmodule-node")]
#[rkyv(derive(Debug))]
pub struct ModuleNode {
    pub id: ModuleId,
    /// Human-readable label (the file path relative to the flake root,
    /// e.g. `"profiles/nixos-attic-cache-warmer/default.nix"`).
    pub label: String,
    /// Hash of the AstGraph this module was lowered from. Two modules
    /// with byte-identical AST contribute identically; the compiler can
    /// memoize on this.
    pub ast_graph_hash: [u8; 32],
    /// Option declarations this module contributes to the schema.
    pub option_decls: Vec<OptionDecl>,
    /// Config-setter fragments. One per logical assignment in the
    /// module's body. `mkMerge` / `mkIf` are pre-split here.
    pub setters: Vec<ConfigSetter>,
    /// Import edges: which other modules this one pulls into the
    /// system. Resolved at parse time so the runtime view is flat.
    pub imports: Vec<ImportEdge>,
    /// Env-prefix bindings captured from the module's outer wrappers
    /// (let-in bindings, with-scope attrsets). Each entry maps an
    /// identifier name to the AST node id whose evaluation produces
    /// the binding's value. The evaluator seeds each setter's
    /// evaluation env with these BEFORE adding `config` — so a setter
    /// body that references `cfg` (bound by an outer `let cfg =
    /// config.foo;`) resolves correctly.
    ///
    /// Two entry kinds today:
    ///   * Named bindings from outer `let ... in BODY` clauses.
    ///   * Synthetic `__with_<n>` entries for each outer `with X;`
    ///     scope, where X's evaluation result is unpacked into the
    ///     env (its attrset attrs become top-level idents).
    ///     `__with_<n>` itself is never directly referenced; it's
    ///     a placeholder so the evaluator knows to unpack the value.
    pub body_env_prefix: Vec<EnvPrefixBinding>,
}

/// One env-prefix binding captured from a module's outer wrapping.
/// See [`ModuleNode::body_env_prefix`].
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
pub struct EnvPrefixBinding {
    /// Identifier name this binding is reachable under. For synthetic
    /// `with X;` entries, this is `"__with_N"` where N is the unwrap
    /// depth — uniquely identifies which scope the entry came from.
    pub name: String,
    /// AST node id whose evaluation produces the binding's value.
    pub value_node_id: super::ast_graph::NodeId,
    /// Kind of binding — let-bound name, or `with`-scope attrset to
    /// be unpacked.
    pub kind: EnvPrefixKind,
}

/// Distinguishes let-bindings (just bind name=value) from
/// with-clauses (unpack the value's attrset attrs as top-level
/// names in the env).
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
)]
#[rkyv(derive(Debug))]
pub enum EnvPrefixKind {
    /// Bound by name: `let foo = …; in BODY`.
    Let,
    /// Unpacked attrset: `with foo; BODY`.
    With,
}

/// One `options.foo.bar = mkOption { ... }` declaration.
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
pub struct OptionDecl {
    /// Dotted path: `["services", "atticd", "enable"]`.
    pub path: Vec<String>,
    /// Type-tag name (e.g. `"bool"`, `"str"`, `"submodule"`). Free-
    /// form for now; tightens to an enum in a later ship when the
    /// type lattice is enumerated.
    pub type_tag: String,
    /// Whether the option carries a default.
    pub has_default: bool,
    /// Human-readable description (the `description` arg of mkOption).
    pub description: Option<String>,
}

/// One config-setter fragment. The worker/wrapper-split shape is
/// captured in the types: `body_ast` is the worker (computes the
/// contribution); `slice` is the wrapper's declared input projection
/// (what the worker reads from `config`).
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
pub struct ConfigSetter {
    pub id: SetterId,
    /// Dotted path being assigned, e.g.
    /// `["services", "atticd", "settings", "listen"]`.
    pub assigns_path: Vec<String>,
    /// Slice of `config` this setter reads. Each entry is a dotted
    /// path. Empty list = "doesn't read config at all" (leaf setter).
    pub slice: Vec<Vec<String>>,
    /// Pointer into the source [`AstGraph`]: the [`AstNodeId`] whose
    /// subgraph evaluates to the assignment's RHS. The IR doesn't
    /// embed the AST — it references it, so the same AST blob backs
    /// every setter that names a node in it.
    pub body_ast_root: AstNodeId,
    /// `mkIf` condition wrapping this assignment, if any. Pointer to
    /// the boolean expression in the AST. `None` = unconditional.
    pub condition_ast_root: Option<AstNodeId>,
    /// `mkOverride` priority. Defaults to 100 (cppnix default). Lower
    /// numbers win — `mkForce` = 50, `mkVMOverride` = 10.
    pub priority: u32,
}

/// One `imports = [ ... ];` edge.
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
pub struct ImportEdge {
    /// Target module id. Resolved at build time, so runtime is O(1).
    pub target: ModuleId,
    /// Optional `mkIf` condition gating the import.
    pub condition_ast_root: Option<AstNodeId>,
}

/// Errors from the module-graph builder.
#[derive(Debug, thiserror::Error)]
pub enum ModuleGraphError {
    #[error("rkyv archive failed: {0}")]
    Archive(String),
    #[error("module {label:?} declared an import path that didn't resolve")]
    UnresolvedImport { label: String },
    #[error("module compiler failed: {source}")]
    Compiler {
        #[from]
        source: crate::module_compiler::ModuleCompilerError,
    },
}

impl ModuleGraph {
    /// Allocate an empty graph. Builders use [`Self::push_module`] +
    /// [`Self::set_root`] to populate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            root_id: 0,
            modules: Vec::new(),
            canonical_hash: ModuleGraphHash { bytes: [0u8; 32] },
        }
    }

    /// Add a module; return its assigned id. First module pushed gets
    /// id 0 (the root by convention; callers should ensure that's the
    /// entrypoint module).
    pub fn push_module(&mut self, node: ModuleNode) -> ModuleId {
        let id = self.modules.len() as ModuleId;
        let mut n = node;
        n.id = id;
        self.modules.push(n);
        id
    }

    /// Set which module id is the root. Defaults to 0; override only
    /// if the build order put the root elsewhere.
    pub fn set_root(&mut self, id: ModuleId) {
        self.root_id = id;
    }

    /// Build a `ModuleGraph` from a slice of `(label, AstGraph)` pairs.
    /// The first pair becomes the root.
    ///
    /// Each module is run through [`crate::module_compiler::compile_module`]
    /// to extract its typed surface (option declarations, config setters
    /// with slice metadata, import edges). Caller-order is preserved —
    /// full BFS-from-root topological discovery + import-target
    /// resolution lands when import paths get typed in a follow-up
    /// ship (today, [`ImportEdge::target`] is the `u32::MAX` sentinel
    /// for unresolved edges).
    ///
    /// # Errors
    ///
    /// - [`ModuleGraphError::Archive`] on rkyv failure during
    ///   subsequent `archive_and_hash` calls.
    /// - Module-compiler errors are surfaced as `ModuleGraphError`
    ///   variants — a module with an unrecognizable root shape causes
    ///   the whole build to fail rather than silently emit a partial
    ///   graph (operators should see the offending file).
    pub fn from_ast_graphs(modules: &[(String, AstGraph)]) -> Result<Self, ModuleGraphError> {
        let mut g = Self::new();
        let mut label_to_id: BTreeMap<String, ModuleId> = BTreeMap::new();
        for (label, ast) in modules {
            let next_id = g.modules.len() as ModuleId;
            let node = crate::module_compiler::compile_module(label, ast, next_id)
                .map_err(|e| ModuleGraphError::Compiler { source: e })?;
            let id = g.push_module(node);
            label_to_id.insert(label.clone(), id);
        }
        Ok(g)
    }

    /// Two-pass archive: serialize → BLAKE3 the bytes → stamp hash →
    /// serialize again. Mirrors `LockfileGraph::archive_and_hash` and
    /// `AstGraph::archive_and_hash`.
    ///
    /// # Errors
    ///
    /// [`ModuleGraphError::Archive`] on rkyv failure.
    pub fn archive_and_hash(mut self) -> Result<(Self, Vec<u8>), ModuleGraphError> {
        let initial = rkyv::to_bytes::<rkyv::rancor::Error>(&self)
            .map_err(|e| ModuleGraphError::Archive(e.to_string()))?;
        let hash = blake3::hash(&initial);
        self.canonical_hash = ModuleGraphHash { bytes: hash.into() };
        let stamped = rkyv::to_bytes::<rkyv::rancor::Error>(&self)
            .map_err(|e| ModuleGraphError::Archive(e.to_string()))?;
        Ok((self, stamped.to_vec()))
    }
}

impl Default for ModuleGraph {
    fn default() -> Self {
        Self::new()
    }
}

/// Bumped on every breaking change to the IR. Today: 1.
pub const SCHEMA_VERSION: u32 = 1;

// ── Lisp fixtures loader ──────────────────────────────────────────

#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defmodule-graph-fixture")]
pub struct ModuleGraphFixture {
    pub name: String,
    #[serde(rename = "moduleCount")]
    pub module_count: u32,
    #[serde(rename = "setterCount")]
    pub setter_count: u32,
    #[serde(rename = "optionCount")]
    pub option_count: u32,
    #[serde(rename = "sliceCount")]
    pub slice_count: u32,
    pub notes: String,
}

pub const CANONICAL_MODULE_GRAPH_FIXTURES_LISP: &str =
    include_str!("../specs/module_graph.lisp");

/// Load every authored fixture.
///
/// # Errors
///
/// Fails if the `.lisp` source can't be parsed.
pub fn load_fixtures() -> Result<Vec<ModuleGraphFixture>, SpecError> {
    crate::loader::load_all::<ModuleGraphFixture>(CANONICAL_MODULE_GRAPH_FIXTURES_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn ast(src: &str) -> AstGraph {
        AstGraph::from_source(src).unwrap()
    }

    #[test]
    fn empty_graph_round_trips() {
        let g = ModuleGraph::new();
        assert_eq!(g.schema_version, SCHEMA_VERSION);
        assert_eq!(g.root_id, 0);
        assert!(g.modules.is_empty());
    }

    #[test]
    fn push_module_assigns_dense_ids() {
        let mut g = ModuleGraph::new();
        let id0 = g.push_module(ModuleNode {
            id: 99, // ignored — push_module overrides
            label: "root".into(),
            ast_graph_hash: [0u8; 32],
            option_decls: Vec::new(),
            setters: Vec::new(),
            imports: Vec::new(),
            body_env_prefix: Vec::new(),
        });
        let id1 = g.push_module(ModuleNode {
            id: 99,
            label: "child".into(),
            ast_graph_hash: [0u8; 32],
            option_decls: Vec::new(),
            setters: Vec::new(),
            imports: Vec::new(),
            body_env_prefix: Vec::new(),
        });
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);
        assert_eq!(g.modules[0].id, 0);
        assert_eq!(g.modules[1].id, 1);
    }

    #[test]
    fn from_ast_graphs_seeds_per_module_hash() {
        let a = ast("{ networking.hostName = \"rio\"; }");
        let b = ast("{ boot.kernelParams = [ \"amd_pstate=active\" ]; }");
        let modules = vec![
            ("root.nix".to_string(), a.clone()),
            ("child.nix".to_string(), b.clone()),
        ];
        let g = ModuleGraph::from_ast_graphs(&modules).unwrap();
        assert_eq!(g.modules.len(), 2);
        assert_eq!(g.modules[0].label, "root.nix");
        assert_eq!(g.modules[0].ast_graph_hash, a.canonical_hash.bytes);
        assert_eq!(g.modules[1].ast_graph_hash, b.canonical_hash.bytes);
    }

    #[test]
    fn archive_and_hash_stamps_canonical_hash() {
        let mut g = ModuleGraph::new();
        g.push_module(ModuleNode {
            id: 0,
            label: "x".into(),
            ast_graph_hash: [1u8; 32],
            option_decls: Vec::new(),
            setters: Vec::new(),
            imports: Vec::new(),
            body_env_prefix: Vec::new(),
        });
        assert_eq!(g.canonical_hash.bytes, [0u8; 32]);
        let (stamped, bytes) = g.archive_and_hash().unwrap();
        assert_ne!(stamped.canonical_hash.bytes, [0u8; 32]);
        assert!(!bytes.is_empty());
    }

    #[test]
    fn archive_is_deterministic_for_same_input() {
        let mk = || {
            let mut g = ModuleGraph::new();
            g.push_module(ModuleNode {
                id: 0,
                label: "y".into(),
                ast_graph_hash: [7u8; 32],
                option_decls: Vec::new(),
                setters: Vec::new(),
                imports: Vec::new(),
            body_env_prefix: Vec::new(),
            });
            g
        };
        let (a, ba) = mk().archive_and_hash().unwrap();
        let (b, bb) = mk().archive_and_hash().unwrap();
        assert_eq!(a.canonical_hash.bytes, b.canonical_hash.bytes);
        assert_eq!(ba, bb);
    }

    #[test]
    fn archive_roundtrips_via_rkyv() {
        let mut g = ModuleGraph::new();
        g.push_module(ModuleNode {
            id: 0,
            label: "rt".into(),
            ast_graph_hash: [3u8; 32],
            option_decls: vec![OptionDecl {
                path: vec!["services".into(), "atticd".into(), "enable".into()],
                type_tag: "bool".into(),
                has_default: true,
                description: Some("enable atticd".into()),
            }],
            setters: vec![ConfigSetter {
                id: 0,
                assigns_path: vec!["services".into(), "atticd".into(), "enable".into()],
                slice: Vec::new(),
                body_ast_root: 0,
                condition_ast_root: None,
                priority: 100,
            }],
            imports: Vec::new(),
            body_env_prefix: Vec::new(),
        });
        let (_, bytes) = g.archive_and_hash().unwrap();
        let archived =
            rkyv::access::<ArchivedModuleGraph, rkyv::rancor::Error>(&bytes).unwrap();
        assert_eq!(archived.modules.len(), 1);
        assert_eq!(archived.modules[0].label.as_str(), "rt");
        assert_eq!(archived.modules[0].option_decls.len(), 1);
        assert_eq!(archived.modules[0].setters.len(), 1);
    }

    #[test]
    fn fixtures_load_from_lisp() {
        let f = load_fixtures().unwrap();
        let names: Vec<_> = f.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"single-module-hostname"));
        assert!(names.contains(&"two-modules-with-slice"));
        assert!(names.contains(&"imports-chain-depth-4"));
    }
}
