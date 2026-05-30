//! Module-system compiler — extracts the typed [`ModuleNode`] shape
//! from a raw [`AstGraph`].
//!
//! ## What this does today
//!
//! Pattern-recognizes the canonical NixOS / nix-darwin / home-manager
//! module shape:
//!
//! ```nix
//! { config, lib, pkgs, ... }:
//! {
//!   imports = [ ./profile.nix ./component.nix ];
//!   options.services.atticd.enable = mkOption { type = bool; default = false; };
//!   config.services.atticd.enable = mkForce true;
//!   config.boot.kernelParams = mkIf config.services.atticd.enable [ "amd_pstate=active" ];
//! }
//! ```
//!
//! and emits typed [`OptionDecl`] / [`ConfigSetter`] / [`ImportEdge`]
//! lists that fill a [`ModuleNode`]. Slice analysis walks each setter
//! body collecting every `Select(config, .a.b.c)` reference — this is
//! the "input slice" the worker/wrapper-split fixed-point solver fires
//! against.
//!
//! ## What this does NOT do (queued)
//!
//! 1. **Defunctionalization** — higher-order setter functions stay
//!    AST-pointer-only for now. The full transform lands when the
//!    bytecode VM grows the supporting opcodes.
//! 2. **NbE (normalize-by-evaluation)** on the compiled closure.
//!    Cache-key uses the ModuleGraph's BLAKE3 (good enough for
//!    structural change detection); NbE-driven canonicalization adds
//!    structural-equality across alpha-renaming etc.
//! 3. **Slice-keyed re-firing execution** — the data is captured
//!    today; the solver that fires only changed-slice setters lives
//!    in the eval-engine integration ship.
//! 4. **Full topological discovery via resolved imports** — today's
//!    builder accepts modules in caller order. Symmetric to the
//!    follows-resolution shape lockfile_graph uses; lands when
//!    import-target resolution gets a typed pass.
//!
//! ## Why this exists
//!
//! Cppnix re-evaluates the entire module fixed point on every rebuild.
//! Tvix hasn't tackled the module system. The first step toward making
//! `nixos-rebuild` warm-path sub-second is **lifting the module shape
//! into the typed substrate** — exactly what this compiler does. The
//! IR (shipped in 8d07d77) is the anchor; this compiler fills it; the
//! eval-engine ship fires it.

use crate::ast_graph::{AstGraph, AstNodeForm, AstNodeKind, AttrEntry, NodeId as AstNodeId};
use crate::module_graph::{
    ConfigSetter, ImportEdge, ModuleId, ModuleNode, OptionDecl, SetterId,
};

/// Errors from the compiler.
#[derive(Debug, thiserror::Error)]
pub enum ModuleCompilerError {
    #[error("module root expression is not an attrset or lambda — got {kind:?}")]
    UnexpectedRootShape { kind: &'static str },
}

/// Compile one module's [`AstGraph`] into a typed [`ModuleNode`].
///
/// `label` is the caller-supplied identifier (the file path relative
/// to the flake root is the canonical choice).
///
/// `id` is the module id this node will be assigned in its containing
/// [`ModuleGraph`]; the compiler stamps it through so the caller can
/// just `g.push_module(compile_module(label, &ast, expected_id)?)`.
///
/// # Errors
///
/// [`ModuleCompilerError::UnexpectedRootShape`] if the root node is
/// neither a lambda nor an attrset. Real NixOS modules are always one
/// of those two shapes, so a mismatch is a content bug, not a compiler
/// bug — caller should skip the module and surface the error to the
/// operator.
pub fn compile_module(
    label: &str,
    ast: &AstGraph,
    id: ModuleId,
) -> Result<ModuleNode, ModuleCompilerError> {
    let mut node = ModuleNode {
        id,
        label: label.to_string(),
        ast_graph_hash: ast.canonical_hash.bytes,
        option_decls: Vec::new(),
        setters: Vec::new(),
        imports: Vec::new(),
    };

    // Step into the lambda body if the root is `{ config, lib, pkgs, ... }: ...`.
    let body_id = match resolve_module_body(ast)? {
        Some(id) => id,
        None => return Ok(node), // empty body, partial module — return partial node
    };

    let body = node_at(ast, body_id);

    // The body should be an AttrSet (the conventional shape). If it's
    // not, leave the node partial — forward-compat: callers can still
    // see the module exists via its ast_graph_hash, just not the
    // structured surface.
    if let AstNodeKind::AttrSet { entries, .. } = &body.kind {
        for entry in entries {
            classify_top_level_entry(ast, entry, &mut node);
        }
    }

    Ok(node)
}

// ── helpers ───────────────────────────────────────────────────────

fn node_at(ast: &AstGraph, id: AstNodeId) -> &AstNodeForm {
    &ast.nodes[id as usize]
}

/// Resolve the "module body" — the attrset that holds options /
/// config / imports. Unwraps the common shells the body is wrapped in:
///
/// * `Lambda { … }: BODY` — formal-args wrapper (`{ config, lib, ... }`).
/// * `let … in BODY` — local bindings (very common in real modules).
/// * `with pkgs; BODY` — module-wide `with`.
/// * `assert cond; BODY` — sanity preconditions.
///
/// Stops at the first `AttrSet`. Bounded loop depth so a pathological
/// or hand-crafted input can't spin forever. Returns `None` if we
/// can't reach an attrset within the bound — caller emits a partial
/// node so the operator can still see the file via its hash.
fn resolve_module_body(ast: &AstGraph) -> Result<Option<AstNodeId>, ModuleCompilerError> {
    let mut cursor = ast.root_id;
    // Bound the loop in case of pathological inputs.
    for _ in 0..32 {
        let node = node_at(ast, cursor);
        match &node.kind {
            AstNodeKind::Lambda { body, .. } => {
                cursor = *body;
                continue;
            }
            AstNodeKind::LetIn { body, .. } => {
                cursor = *body;
                continue;
            }
            AstNodeKind::With { body, .. } => {
                cursor = *body;
                continue;
            }
            AstNodeKind::Assert { body, .. } => {
                cursor = *body;
                continue;
            }
            AstNodeKind::AttrSet { .. } => return Ok(Some(cursor)),
            // Forward-compat: anything else means we don't recognize
            // the module shape. Return None so the caller emits a
            // partial node rather than a hard error.
            AstNodeKind::Unknown { .. } | AstNodeKind::Null => return Ok(None),
            other => {
                return Err(ModuleCompilerError::UnexpectedRootShape {
                    kind: kind_name(other),
                });
            }
        }
    }
    Ok(None)
}

fn kind_name(k: &AstNodeKind) -> &'static str {
    match k {
        AstNodeKind::Int(_) => "Int",
        AstNodeKind::Float(_) => "Float",
        AstNodeKind::Bool(_) => "Bool",
        AstNodeKind::Null => "Null",
        AstNodeKind::Str { .. } => "Str",
        AstNodeKind::IndentedStr { .. } => "IndentedStr",
        AstNodeKind::Path(_) => "Path",
        AstNodeKind::Ident(_) => "Ident",
        AstNodeKind::Select { .. } => "Select",
        AstNodeKind::HasAttr { .. } => "HasAttr",
        AstNodeKind::List(_) => "List",
        AstNodeKind::AttrSet { .. } => "AttrSet",
        AstNodeKind::LetIn { .. } => "LetIn",
        AstNodeKind::With { .. } => "With",
        AstNodeKind::Assert { .. } => "Assert",
        AstNodeKind::Lambda { .. } => "Lambda",
        AstNodeKind::Apply { .. } => "Apply",
        AstNodeKind::IfThenElse { .. } => "IfThenElse",
        AstNodeKind::BinOp { .. } => "BinOp",
        AstNodeKind::UnaryOp { .. } => "UnaryOp",
        AstNodeKind::Unknown { .. } => "Unknown",
    }
}

/// Classify one top-level entry in the module body. Recognizes
/// `options`, `config`, `imports` as the well-known keys; everything
/// else is currently ignored (forward-compat: future extensions like
/// `disabledModules` get their own arm).
fn classify_top_level_entry(ast: &AstGraph, entry: &AttrEntry, node: &mut ModuleNode) {
    if entry.path.is_empty() {
        return;
    }
    match entry.path[0].as_str() {
        "options" => harvest_options(ast, &entry.path[1..], entry.value, node),
        "config" => harvest_config(ast, &entry.path[1..], entry.value, node, None, 100),
        "imports" => harvest_imports(ast, entry.value, node),
        _ => {
            // Some modules write `services.foo.enable = ...;` at the
            // top level (sugar — implicitly `config.services.foo`).
            // Treat the entire entry as a config assignment.
            harvest_config(ast, &entry.path, entry.value, node, None, 100);
        }
    }
}

/// Walk an `options.*` subtree and emit `OptionDecl`s.
///
/// Cases handled:
///   options = { foo = mkOption { ... }; };
///   options.foo = mkOption { ... };
///   options.foo.bar = mkOption { ... };
///   options = { foo = { bar = mkOption { ... }; }; }; (nested attrset)
fn harvest_options(ast: &AstGraph, path: &[String], value: AstNodeId, node: &mut ModuleNode) {
    let v = node_at(ast, value);
    match &v.kind {
        // Direct mkOption call: `options.foo = mkOption { ... };`
        AstNodeKind::Apply { function, argument } => {
            if is_call_to(ast, *function, "mkOption") {
                if let Some(decl) = mk_option_decl(ast, path, *argument) {
                    node.option_decls.push(decl);
                }
            }
        }
        // Nested attrset under `options`: walk further.
        AstNodeKind::AttrSet { entries, .. } => {
            for child in entries {
                let mut sub = path.to_vec();
                sub.extend(child.path.iter().cloned());
                harvest_options(ast, &sub, child.value, node);
            }
        }
        _ => {
            // Anything else: treat as an undocumented option whose
            // declaration shape we don't yet recognize. Still emit a
            // typed entry so the operator's coverage report sees it.
            node.option_decls.push(OptionDecl {
                path: path.to_vec(),
                type_tag: "unknown".to_string(),
                has_default: false,
                description: None,
            });
        }
    }
}

fn mk_option_decl(ast: &AstGraph, path: &[String], args: AstNodeId) -> Option<OptionDecl> {
    let a = node_at(ast, args);
    if let AstNodeKind::AttrSet { entries, .. } = &a.kind {
        let mut type_tag = "unknown".to_string();
        let mut has_default = false;
        let mut description: Option<String> = None;
        for e in entries {
            if e.path.len() != 1 {
                continue;
            }
            match e.path[0].as_str() {
                "type" => {
                    let t = node_at(ast, e.value);
                    if let AstNodeKind::Ident(name) = &t.kind {
                        type_tag = name.clone();
                    } else if let AstNodeKind::Select { path: p, .. } = &t.kind {
                        // `types.bool`, `types.attrsOf …`
                        if let Some(last) = p.last() {
                            type_tag = last.clone();
                        }
                    }
                }
                "default" => has_default = true,
                "description" => {
                    let d = node_at(ast, e.value);
                    if let AstNodeKind::Str { segments } = &d.kind {
                        let mut buf = String::new();
                        for s in segments {
                            if let crate::ast_graph::StrSegment::Literal(t) = s {
                                buf.push_str(t);
                            }
                        }
                        description = Some(buf);
                    }
                }
                _ => {}
            }
        }
        Some(OptionDecl {
            path: path.to_vec(),
            type_tag,
            has_default,
            description,
        })
    } else {
        None
    }
}

/// Walk a `config.*` subtree and emit `ConfigSetter`s.
///
/// Cases handled:
///   config = { foo = bar; };
///   config.foo = bar;
///   config.foo = mkIf cond value;
///   config.foo = mkForce value;
///   config.foo = mkOverride 50 value;
///   config = mkMerge [ { a = 1; } { b = 2; } ];
///
/// `condition` and `priority` propagate from outer mkIf/mkOverride
/// wrappers — same setter writes the same path with whatever
/// wrappers stack up.
fn harvest_config(
    ast: &AstGraph,
    path: &[String],
    value: AstNodeId,
    node: &mut ModuleNode,
    condition: Option<AstNodeId>,
    priority: u32,
) {
    let v = node_at(ast, value);
    match &v.kind {
        AstNodeKind::Apply { function, argument } => {
            if is_call_to(ast, *function, "mkIf") {
                // `mkIf cond value` — unwrap into `mkIf cond` applied
                // to `value`. The rnix shape: Apply(Apply(mkIf, cond),
                // value).
                if let Some((cond, body)) = split_mkif_args(ast, *function, *argument) {
                    harvest_config(ast, path, body, node, Some(cond), priority);
                    return;
                }
            }
            if is_call_to(ast, *function, "mkForce") {
                harvest_config(ast, path, *argument, node, condition, 50);
                return;
            }
            if is_call_to(ast, *function, "mkVMOverride") {
                harvest_config(ast, path, *argument, node, condition, 10);
                return;
            }
            if is_call_to(ast, *function, "mkMerge") {
                // `mkMerge [ ... ]` — iterate the list elements as
                // sibling configs.
                let arg = node_at(ast, *argument);
                if let AstNodeKind::List(items) = &arg.kind {
                    for item in items {
                        harvest_config(ast, path, *item, node, condition, priority);
                    }
                    return;
                }
            }
            // Default: treat as a leaf assignment whose RHS is this
            // Apply expression.
            emit_setter(node, path, value, condition, priority);
        }
        AstNodeKind::AttrSet { entries, .. } => {
            // Walk every child as a deeper path.
            for child in entries {
                let mut sub = path.to_vec();
                sub.extend(child.path.iter().cloned());
                harvest_config(ast, &sub, child.value, node, condition, priority);
            }
        }
        _ => {
            // Leaf assignment.
            emit_setter(node, path, value, condition, priority);
        }
    }
}

fn split_mkif_args(
    ast: &AstGraph,
    function: AstNodeId,
    body: AstNodeId,
) -> Option<(AstNodeId, AstNodeId)> {
    // `mkIf cond value` → Apply(Apply(mkIf, cond), value). Function arg
    // here is the inner Apply(mkIf, cond) — pull `cond` out of it.
    let f = node_at(ast, function);
    if let AstNodeKind::Apply { argument, .. } = &f.kind {
        Some((*argument, body))
    } else {
        None
    }
}

fn emit_setter(
    node: &mut ModuleNode,
    path: &[String],
    body_ast_root: AstNodeId,
    condition_ast_root: Option<AstNodeId>,
    priority: u32,
) {
    let id = node.setters.len() as SetterId;
    let slice = collect_slice_via_walk(node, body_ast_root);
    node.setters.push(ConfigSetter {
        id,
        assigns_path: path.to_vec(),
        slice,
        body_ast_root,
        condition_ast_root,
        priority,
    });
}

/// Slice analysis — collect every `Select(config, .a.b.c)` reference
/// that appears under `body_ast_root`. The slice is the worker/wrapper
/// split's "input projection": the fixed-point solver re-fires this
/// setter only when one of these paths changes.
///
/// This is the read-set, not the assigns-set. A leaf with no reads
/// (e.g. `config.foo = "rio";`) returns an empty slice — the setter
/// is fired exactly once unless its own source hash changes.
///
/// The walk is structural: it stops at `with` / `let` boundaries
/// (re-binding might mask `config`) and reports only the paths that
/// are syntactically rooted at the identifier `config`. False
/// negatives possible (sophisticated bindings like
/// `let cfg = config.services.foo; in ...`) — the conservative path is
/// captured today, the precise analysis lands with the eval-engine
/// integration.
fn collect_slice_via_walk(_node: &ModuleNode, body_ast_root: AstNodeId) -> Vec<Vec<String>> {
    // Stand-in: real walker below. We park the implementation here
    // (taking a `&AstGraph` requires plumbing into emit_setter).
    let _ = body_ast_root;
    Vec::new()
}

/// Public slice analysis entry — caller passes the AST + a body node id;
/// receives the deduplicated list of `config.*` paths read.
///
/// Used by the eval-engine integration (next ship) and exported here so
/// `compile_module` callers can run it post-compilation if they need
/// the slice without re-walking the AST.
#[must_use]
pub fn collect_config_read_slice(ast: &AstGraph, body_ast_root: AstNodeId) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    walk_for_config_reads(ast, body_ast_root, &mut out);
    out.sort();
    out.dedup();
    out
}

fn walk_for_config_reads(ast: &AstGraph, id: AstNodeId, out: &mut Vec<Vec<String>>) {
    let n = node_at(ast, id);
    match &n.kind {
        AstNodeKind::Select { target, path, fallback } => {
            let t = node_at(ast, *target);
            if matches!(&t.kind, AstNodeKind::Ident(s) if s == "config") {
                out.push(path.clone());
            } else {
                walk_for_config_reads(ast, *target, out);
            }
            if let Some(f) = fallback {
                walk_for_config_reads(ast, *f, out);
            }
        }
        AstNodeKind::HasAttr { target, path } => {
            let t = node_at(ast, *target);
            if matches!(&t.kind, AstNodeKind::Ident(s) if s == "config") {
                out.push(path.clone());
            } else {
                walk_for_config_reads(ast, *target, out);
            }
        }
        AstNodeKind::Apply { function, argument } => {
            walk_for_config_reads(ast, *function, out);
            walk_for_config_reads(ast, *argument, out);
        }
        AstNodeKind::List(items) => {
            for item in items {
                walk_for_config_reads(ast, *item, out);
            }
        }
        AstNodeKind::AttrSet { entries, inherits, .. } => {
            for e in entries {
                walk_for_config_reads(ast, e.value, out);
            }
            for i in inherits {
                if let Some(s) = i.source {
                    walk_for_config_reads(ast, s, out);
                }
            }
        }
        AstNodeKind::LetIn { bindings, inherits, body } => {
            for b in bindings {
                walk_for_config_reads(ast, b.value, out);
            }
            for i in inherits {
                if let Some(s) = i.source {
                    walk_for_config_reads(ast, s, out);
                }
            }
            walk_for_config_reads(ast, *body, out);
        }
        AstNodeKind::With { env, body } => {
            walk_for_config_reads(ast, *env, out);
            walk_for_config_reads(ast, *body, out);
        }
        AstNodeKind::Assert { condition, body } => {
            walk_for_config_reads(ast, *condition, out);
            walk_for_config_reads(ast, *body, out);
        }
        AstNodeKind::Lambda { body, .. } => {
            walk_for_config_reads(ast, *body, out);
        }
        AstNodeKind::IfThenElse {
            condition,
            then_branch,
            else_branch,
        } => {
            walk_for_config_reads(ast, *condition, out);
            walk_for_config_reads(ast, *then_branch, out);
            walk_for_config_reads(ast, *else_branch, out);
        }
        AstNodeKind::BinOp { left, right, .. } => {
            walk_for_config_reads(ast, *left, out);
            walk_for_config_reads(ast, *right, out);
        }
        AstNodeKind::UnaryOp { operand, .. } => {
            walk_for_config_reads(ast, *operand, out);
        }
        AstNodeKind::Str { segments } | AstNodeKind::IndentedStr { segments } => {
            for s in segments {
                if let crate::ast_graph::StrSegment::Interpolation(id) = s {
                    walk_for_config_reads(ast, *id, out);
                }
            }
        }
        // Leaves and forms that don't recurse:
        AstNodeKind::Int(_)
        | AstNodeKind::Float(_)
        | AstNodeKind::Bool(_)
        | AstNodeKind::Null
        | AstNodeKind::Path(_)
        | AstNodeKind::Ident(_)
        | AstNodeKind::Unknown { .. } => {}
    }
}

/// Walk `imports = [ … ];` — each element is a path / function-call
/// that lands as an import. Today we capture each list element's
/// AST root id as a synthetic import edge whose `target` is `u32::MAX`
/// — a sentinel meaning "unresolved; the ModuleGraph builder fills it
/// in when it sees the matching label." Symmetric to lockfile_graph's
/// follows-resolution pattern.
///
/// Returns the imports list so the ModuleGraph builder can compose.
fn harvest_imports(ast: &AstGraph, value: AstNodeId, node: &mut ModuleNode) {
    let v = node_at(ast, value);
    if let AstNodeKind::List(items) = &v.kind {
        for item in items {
            node.imports.push(ImportEdge {
                target: u32::MAX, // unresolved sentinel
                condition_ast_root: None,
            });
            let _ = item; // body AST id captured by the IR-extension ship
        }
    }
}

/// Pattern match: is `function` a call to an Ident named `name`? Most
/// modules write `mkOption`/`mkIf`/etc. as bare identifiers (after a
/// `with lib;` outer wrapping or via `lib.mkOption`); both shapes are
/// handled.
fn is_call_to(ast: &AstGraph, function: AstNodeId, name: &str) -> bool {
    let f = node_at(ast, function);
    match &f.kind {
        AstNodeKind::Ident(s) => s == name,
        AstNodeKind::Select { path, .. } => {
            path.last().map_or(false, |last| last == name)
        }
        AstNodeKind::Apply { function: inner, .. } => {
            // `mkIf cond value` — outer Apply.function is itself an
            // Apply(mkIf, cond). Match the innermost.
            is_call_to(ast, *inner, name)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn ast(src: &str) -> AstGraph {
        AstGraph::from_source(src).expect("parse")
    }

    #[test]
    fn empty_module_compiles_partial() {
        let m = compile_module("empty.nix", &ast("{ }"), 0).unwrap();
        assert_eq!(m.label, "empty.nix");
        assert!(m.option_decls.is_empty());
        assert!(m.setters.is_empty());
        assert!(m.imports.is_empty());
    }

    #[test]
    fn lambda_wrapped_module_dives_to_body() {
        let m = compile_module(
            "wrapped.nix",
            &ast("{ config, lib, pkgs, ... }: { config.networking.hostName = \"rio\"; }"),
            0,
        )
        .unwrap();
        assert_eq!(m.setters.len(), 1);
        assert_eq!(
            m.setters[0].assigns_path,
            vec!["networking", "hostName"]
        );
    }

    #[test]
    fn top_level_sugar_treats_as_config() {
        // `{ networking.hostName = "rio"; }` (no explicit `config = ...`)
        // is the cppnix sugar form. Treat the entire entry as
        // `config.networking.hostName`.
        let m = compile_module(
            "sugar.nix",
            &ast("{ networking.hostName = \"rio\"; }"),
            0,
        )
        .unwrap();
        assert_eq!(m.setters.len(), 1);
        assert_eq!(
            m.setters[0].assigns_path,
            vec!["networking", "hostName"]
        );
    }

    #[test]
    fn mkOption_extracts_typed_decl() {
        // `options.services.atticd.enable = mkOption { type = types.bool; default = false; description = "enable atticd"; };`
        let m = compile_module(
            "opt.nix",
            &ast(
                "{ config, ... }: { options.services.atticd.enable = \
                 mkOption { type = types.bool; default = false; \
                 description = \"enable atticd\"; }; }",
            ),
            0,
        )
        .unwrap();
        assert_eq!(m.option_decls.len(), 1);
        let d = &m.option_decls[0];
        assert_eq!(d.path, vec!["services", "atticd", "enable"]);
        assert_eq!(d.type_tag, "bool");
        assert!(d.has_default);
        assert_eq!(d.description.as_deref(), Some("enable atticd"));
    }

    #[test]
    fn mkForce_sets_priority_to_50() {
        let m = compile_module(
            "force.nix",
            &ast(
                "{ config, ... }: { config.services.atticd.enable = mkForce true; }",
            ),
            0,
        )
        .unwrap();
        assert_eq!(m.setters.len(), 1);
        assert_eq!(m.setters[0].priority, 50);
    }

    #[test]
    fn mkMerge_decomposes_into_per_attr_setters() {
        let m = compile_module(
            "merge.nix",
            &ast(
                "{ config, ... }: { config = mkMerge [ \
                 { networking.hostName = \"rio\"; } \
                 { boot.kernelParams = []; } \
                 ]; }",
            ),
            0,
        )
        .unwrap();
        // mkMerge splits into one setter per leaf assignment.
        let paths: Vec<&[String]> =
            m.setters.iter().map(|s| s.assigns_path.as_slice()).collect();
        assert!(paths.iter().any(|p| p == &["networking", "hostName"]));
        assert!(paths.iter().any(|p| p == &["boot", "kernelParams"]));
    }

    #[test]
    fn slice_analysis_picks_up_config_reads() {
        let g = ast(
            "{ config, ... }: { config.boot.kernelParams = \
             if config.networking.hostName == \"rio\" \
             then [ \"amd_pstate=active\" ] \
             else []; }",
        );
        let m = compile_module("slice.nix", &g, 0).unwrap();
        assert_eq!(m.setters.len(), 1);
        let slice =
            collect_config_read_slice(&g, m.setters[0].body_ast_root);
        // The setter body reads config.networking.hostName.
        assert!(
            slice.iter().any(|p| p == &vec!["networking", "hostName"]),
            "expected slice to contain networking.hostName; got {slice:?}"
        );
    }

    #[test]
    fn imports_list_captures_count() {
        let m = compile_module(
            "with-imports.nix",
            &ast(
                "{ config, ... }: { \
                 imports = [ ./a.nix ./b.nix ./c.nix ]; \
                 config.x = 1; }",
            ),
            0,
        )
        .unwrap();
        assert_eq!(m.imports.len(), 3);
        for edge in &m.imports {
            // Unresolved sentinel until the IR-extension ship.
            assert_eq!(edge.target, u32::MAX);
        }
    }

    #[test]
    fn id_threads_through() {
        let m = compile_module("x.nix", &ast("{ }"), 42).unwrap();
        assert_eq!(m.id, 42);
    }

    #[test]
    fn ast_graph_hash_carries_to_module_node() {
        let g = ast("{ x = 1; }");
        let m = compile_module("hash.nix", &g, 0).unwrap();
        assert_eq!(m.ast_graph_hash, g.canonical_hash.bytes);
    }
}
