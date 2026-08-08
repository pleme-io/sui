//! Flake evaluation pipeline.
//!
//! Implements the in-process equivalent of `nix eval --raw '(builtins.getFlake
//! "<dir>")'` for path-based flake references.

use crate::value::*;

thread_local! {
    pub(crate) static FLAKE_EVAL_DEPTH: std::cell::RefCell<u32> = const { std::cell::RefCell::new(0) };
}

pub(crate) const MAX_FLAKE_EVAL_DEPTH: u32 = 50;

/// The authoritative lock context threaded through transitive-input
/// resolution.  Holds the ROOT flake's lock (shared, immutable) plus the
/// *node name* of the flake currently being evaluated.
///
/// CppNix pins a flake's ENTIRE transitive input closure in the root lock's
/// node graph and passes each sub-flake its inputs as resolved by THAT graph
/// (walking `follows`), never letting a sub-flake re-resolve from its own
/// `flake.lock`.  Threading this context is what makes sui's transitive
/// resolution byte-identical to nix — the marquee root cause where sui read
/// `ishou`'s own lock (`substrate = fcd35143…`) instead of honoring the root
/// lock's `ishou.inputs.substrate = ["substrate"]` follows edge (→ root's
/// `substrate_5 = b2802c62…`), diverging every downstream drvPath.
#[derive(Clone)]
struct FlakeContext {
    lock: std::rc::Rc<sui_compat::flake::FlakeLock>,
    /// Node name of the flake at `flake_dir` in `lock`'s node graph.
    node_name: String,
}

/// Evaluate a flake directory — reads flake.nix, parses flake.lock, resolves
/// inputs, calls `outputs(inputs)`, and returns the merged result attrset.
///
/// This is the native implementation of `builtins.getFlake` for path-based
/// references.  External callers (orchestrate, CLI) can use this to evaluate
/// a local flake without shelling out to `nix eval`.
///
/// This is the ROOT entrypoint: it reads `flake_dir/flake.lock` as the
/// authoritative closure and evaluates from its root node.  Transitive inputs
/// are resolved against THIS lock (see [`FlakeContext`]), never their own.
pub fn evaluate_flake(flake_dir: &std::path::Path) -> Result<Value, EvalError> {
    evaluate_flake_ctx(flake_dir, None)
}

/// Depth-guarded flake evaluation with an optional inherited lock context.
///
/// `ctx = None` ⇒ the ROOT flake: read `flake_dir/flake.lock`.
/// `ctx = Some(_)` ⇒ a transitive input: resolve its inputs from the inherited
/// root lock at the given node name; the sub-flake's own lock is ignored.
fn evaluate_flake_ctx(
    flake_dir: &std::path::Path,
    ctx: Option<FlakeContext>,
) -> Result<Value, EvalError> {
    let depth = FLAKE_EVAL_DEPTH.with(|d| {
        let mut d = d.borrow_mut();
        *d += 1;
        *d
    });

    if depth > MAX_FLAKE_EVAL_DEPTH {
        FLAKE_EVAL_DEPTH.with(|d| *d.borrow_mut() -= 1);
        return Err(EvalError::RecursionLimit(
            format!(
                "maximum flake evaluation depth ({MAX_FLAKE_EVAL_DEPTH}) exceeded at {}",
                flake_dir.display()
            ),
        ));
    }

    let result = evaluate_flake_inner(flake_dir, ctx);
    FLAKE_EVAL_DEPTH.with(|d| *d.borrow_mut() -= 1);
    result
}

fn evaluate_flake_inner(
    flake_dir: &std::path::Path,
    ctx: Option<FlakeContext>,
) -> Result<Value, EvalError> {
    let flake_nix = flake_dir.join("flake.nix");
    let flake_lock_path = flake_dir.join("flake.lock");

    // 1. Read and evaluate flake.nix.
    let source = std::fs::read_to_string(&flake_nix).map_err(|e| {
        EvalError::IoError {
            context: format!("getFlake: {}", flake_nix.display()),
            message: e.to_string(),
        }
    })?;
    let _flake_file_guard = crate::eval::push_eval_file(flake_nix.clone());
    let flake_value = crate::eval::eval_with_file(&source, Some(flake_nix.clone()))?;
    let flake_attrs = flake_value.to_attrs()?.clone();

    // 2. Pull out the outputs function (required by every flake).
    let outputs_value = flake_attrs
        .get("outputs")
        .ok_or_else(|| EvalError::AttrNotFound("outputs".into()))?
        .clone();
    let outputs_fn = crate::eval::force_value(&outputs_value)?;

    // 3. Establish the authoritative lock context.
    //
    // ROOT invocation (`ctx = None`): read THIS dir's flake.lock — it is the
    // authoritative closure for the whole tree.  TRANSITIVE invocation
    // (`ctx = Some`): INHERIT the root lock + this input's node name; the
    // sub-flake's own flake.lock is deliberately NOT read (that was the
    // divergence — a sub-flake re-resolving its inputs from its own pins
    // instead of the root lock's `follows`-redirected node graph).
    let (lock, current_node): (Option<std::rc::Rc<sui_compat::flake::FlakeLock>>, String) =
        match &ctx {
            Some(c) => (Some(c.lock.clone()), c.node_name.clone()),
            None => {
                if flake_lock_path.exists() {
                    let lock_content =
                        std::fs::read_to_string(&flake_lock_path).map_err(|e| {
                            EvalError::IoError {
                                context: format!("getFlake: {}", flake_lock_path.display()),
                                message: e.to_string(),
                            }
                        })?;
                    let parsed = sui_compat::flake::FlakeLock::parse(&lock_content).map_err(
                        |e| EvalError::TypeError(format!("getFlake: invalid flake.lock: {e}")),
                    )?;
                    let root = parsed.root.clone();
                    (Some(std::rc::Rc::new(parsed)), root)
                } else {
                    (None, String::new())
                }
            }
        };

    // 3b. Create the content-addressed input fetcher.
    let fetcher = crate::fetcher::InputFetcher::new();

    // 4. Resolve every direct input of the CURRENT node against the root lock.
    let self_path = flake_dir.to_string_lossy().to_string();
    let mut resolved_inputs = NixAttrs::new();

    if let Some(ref lock) = lock {
            // Resolved `(input_name, target_node_name)` edges of the current
            // node in the ROOT lock's graph — `follows` already redirected.
            let edges = lock.node_input_edges(&current_node);
            for (input_name, target_node_name) in edges {
                let Some(node) = lock.nodes.get(&target_node_name) else {
                    continue;
                };

                let mut input_val = NixAttrs::new();

                // `out_path` is the cppnix `/nix/store/<narhash>-source`
                // STORE-PATH STRING the input's `outPath`/`sourceInfo`
                // must expose (byte-parity).  `read_dir` is the ACTUAL
                // on-disk directory the tree lives at — the sui fetcher
                // cache (`~/.cache/sui/inputs/…`) for a fetched input, or
                // the literal path for a `type = "path"` input.  These two
                // DIFFER for fetched inputs: sui computes the store-path
                // string via `nar_hash_source_tree` but does NOT copy the
                // tree into `/nix/store` at that path, so reading
                // `flake.nix` / recursing via `evaluate_flake` MUST use
                // `read_dir`, never `out_path` (the marquee darwin root
                // that made every un-materialized transitive input —
                // blackmatter-vpn, …-tailscale — silently drop its flake
                // outputs and surface as `AttrNotFound("darwinModules")`).
                let mut read_dir: Option<std::path::PathBuf> = None;
                let source_out_path = if let Some(ref locked) = node.locked {
                    if locked.source_type == "path" {
                        let p = locked.path.clone().unwrap_or_default();
                        read_dir = Some(std::path::PathBuf::from(&p));
                        p
                    } else {
                        // Byte-parity root (marquee darwin proof, 2026-07-11):
                        // CppNix copies every fetched flake input tree INTO the
                        // nix store as `/nix/store/<narhash>-source` and exposes
                        // THAT store path as the input's `outPath` — the same
                        // step `self` already runs below (§4c
                        // `nar_hash_source_tree`).  The transitive inputs
                        // previously used the raw fetcher CACHE path
                        // (`~/.cache/sui/inputs/…`) verbatim, so any system
                        // config that embeds `nixpkgs.source` into a derivation
                        // (nix-darwin's `/etc/nix/registry.json`, NIX_PATH)
                        // diverged from cppnix at the toplevel drvPath while the
                        // whole module fixpoint matched byte-for-byte.  Mirror
                        // `self`: NAR-hash the fetched tree to its cppnix
                        // `-source` store path.
                        match fetcher.fetch(locked) {
                            Ok(fetched_path) => {
                                read_dir = Some(fetched_path.clone());
                                match sui_compat::source::nar_hash_source_tree(
                                    &fetched_path,
                                    "source",
                                ) {
                                    Ok(sh) => sh.store_path,
                                    Err(e) => {
                                        return Err(EvalError::TypeError(format!(
                                            "nar-hashing flake input '{input_name}' tree at {}: {e}",
                                            fetched_path.display(),
                                        )));
                                    }
                                }
                            }
                            Err(e) => {
                                return Err(EvalError::IoError {
                                    context: format!("fetch flake input '{input_name}'"),
                                    message: e.to_string(),
                                });
                            }
                        }
                    }
                } else {
                    // No `locked` section for this node, so there is nothing to
                    // fetch and no store path to compute. This used to emit
                    // `/nix/store/flake-input-<name>` — a path that does not
                    // exist, yet which PASSES the `starts_with("/nix/store/")`
                    // tests below and so acquired copy-to-store string context
                    // and an input-source registration, masquerading as a real
                    // store reference until it failed as an opaque ENOENT.
                    //
                    // The sentinel is now self-describing and deliberately NOT
                    // store-shaped, so the two guards below skip it and any use
                    // names the input it came from. See `theory/BALIZA-PLAN.md`
                    // §2.5 (measured on rio, 2026-08-08).
                    format!("<unresolved-flake-input:{input_name}>")
                };

                // ── `dir=` SUBFLAKES ───────────────────────────────────────
                // A flake input may name a flake in a SUBDIRECTORY of its
                // source (`github:owner/repo?dir=sub`, carried as `dir` on the
                // locked ref). CppNix then exposes two DIFFERENT paths:
                //
                //   sourceInfo.outPath = /nix/store/<narhash>-source
                //   outPath            = /nix/store/<narhash>-source/<dir>
                //
                // and reads `flake.nix` from the subdirectory.
                //
                // `dir` was PARSED (`sui-compat::flake`, with tests asserting it
                // round-trips) and then consumed NOWHERE — so sui evaluated the
                // repo-ROOT `flake.nix` for every subflake input. The field
                // existed, its test passed, the behaviour was absent.
                //
                // Measured on rio 2026-08-08 (`theory/BALIZA-PLAN.md` §2.5):
                // `blue-bidamas` is `github:pleme-io/blue?dir=bidamas`, whose own
                // `bidamas/flake.nix` declares ONLY `nixpkgs` (outputs are
                // `{ self, nixpkgs }`), while blue's ROOT flake.nix also declares
                // `substrate`. sui read the root, saw a `substrate` input the
                // lock correctly had no edge for, and the failure surfaced far
                // away as a non-existent store path. CppNix on the same input:
                // `outPath = <root>/bidamas`, `sourceInfo.outPath = <root>`.
                //
                // `sourceInfo` keeps the ROOT; only `outPath` and the on-disk
                // read directory descend into `dir`.
                let subdir = node.locked.as_ref().and_then(|l| l.dir.clone());
                let out_path = match subdir {
                    Some(ref d) if !d.is_empty() => {
                        read_dir = read_dir.map(|p| p.join(d));
                        format!("{source_out_path}/{d}")
                    }
                    _ => source_out_path.clone(),
                };
                // Register the `outPath` → real-tree mapping so any read the
                // flake's own Nix code issues under this input's `-source`
                // store path (`import "${input.outPath}/lib/foo.nix"`,
                // `readFile`, `pathExists`, `readDir`, `builtins.path`)
                // resolves against `read_dir` (the fetcher cache / literal
                // path) instead of the un-materialized store path — while the
                // store-path STRING flowing through eval stays byte-correct.
                // (The prior peel special-cased only recursing into
                // `flake.nix`; this generalizes to ALL `${outPath}/subpath`
                // reads.)
                if out_path.starts_with("/nix/store/")
                    && let Some(ref rd) = read_dir {
                        crate::path::register_input_source(
                            std::path::Path::new(&out_path),
                            rd,
                        );
                    }

                // A flake input's `outPath` is a `/nix/store/…-source` store
                // reference: it must carry copy-to-store STRING CONTEXT so that,
                // when a downstream derivation embeds it (nix-darwin's
                // `registry.json` `to.path`), the ATerm gains the matching
                // `source` inputSrc — cppnix records exactly this, and the
                // parity-bisect on the darwin toplevel flagged it as the
                // `nix-only=["source"]` inputSrc gap.  Non-store paths (a
                // `type = "path"` input, the pre-fetch placeholder) stay
                // context-free.
                let out_path_val = if out_path.starts_with("/nix/store/") {
                    let mut ctx = crate::value::StringContext::new();
                    ctx.add_plain(out_path.as_str());
                    Value::String(std::rc::Rc::new(
                        crate::value::NixString::with_context(out_path.as_str(), ctx),
                    ))
                } else {
                    Value::string(out_path.clone())
                };
                input_val.insert("outPath".to_string(), out_path_val.clone());

                if let Some(ref locked) = node.locked {
                    if let Some(ref rev) = locked.rev {
                        input_val.insert("rev".to_string(), Value::string(rev.clone()));
                        let short: String = rev.chars().take(7).collect();
                        input_val.insert("shortRev".to_string(), Value::string(short));
                    }
                    if let Some(ref nar_hash) = locked.nar_hash {
                        input_val.insert(
                            "narHash".to_string(),
                            Value::string(nar_hash.clone()),
                        );
                    }
                    if let Some(last_modified) = locked.last_modified {
                        input_val.insert(
                            "lastModified".to_string(),
                            Value::Int(last_modified as i64),
                        );
                    }

                    let mut source_info = NixAttrs::new();
                    // `sourceInfo.outPath` is the SOURCE ROOT, never the `dir=`
                    // subdirectory — cppnix keeps the two distinct, and only
                    // `outPath` above descends. For a non-subflake input the two
                    // are equal, so this is a no-op there.
                    source_info.insert(
                        "outPath".to_string(),
                        if source_out_path.starts_with("/nix/store/") {
                            let mut ctx = crate::value::StringContext::new();
                            ctx.add_plain(source_out_path.as_str());
                            Value::String(std::rc::Rc::new(
                                crate::value::NixString::with_context(
                                    source_out_path.as_str(),
                                    ctx,
                                ),
                            ))
                        } else {
                            Value::string(source_out_path.clone())
                        },
                    );
                    if let Some(ref rev) = locked.rev {
                        source_info.insert("rev".to_string(), Value::string(rev.clone()));
                    }
                    if let Some(ref nar_hash) = locked.nar_hash {
                        source_info.insert(
                            "narHash".to_string(),
                            Value::string(nar_hash.clone()),
                        );
                    }
                    if let Some(last_modified) = locked.last_modified {
                        source_info.insert(
                            "lastModified".to_string(),
                            Value::Int(last_modified as i64),
                        );
                    }
                    input_val.insert("sourceInfo".to_string(), Value::Attrs(Rc::new(source_info)));
                }

                let is_flake = node.flake.unwrap_or(true);
                if is_flake {
                    // Read `flake.nix` and recurse from the ACTUAL on-disk
                    // tree (`read_dir` — the fetcher cache / literal path),
                    // NOT the `out_path` STORE-PATH STRING which sui does
                    // not materialize into `/nix/store`.  Fall back to
                    // `out_path` only when it genuinely exists (e.g. a store
                    // path already materialized by a prior real nix build).
                    let eval_dir: std::path::PathBuf = match &read_dir {
                        Some(d) if d.join("flake.nix").exists() => d.clone(),
                        _ => std::path::PathBuf::from(&out_path),
                    };
                    let has_flake_nix = eval_dir.join("flake.nix").exists();
                    if has_flake_nix {
                        let immediate = input_val;
                        let dir = eval_dir;
                        // Recurse with the INHERITED root lock at the target
                        // input's node name — never re-reading the sub-flake's
                        // own flake.lock.  This is the byte-parity fix: the
                        // sub-flake's transitive inputs resolve against the one
                        // authoritative closure (with `follows` redirection),
                        // exactly as CppNix does.
                        let child_ctx = FlakeContext {
                            lock: lock.clone(),
                            node_name: target_node_name.clone(),
                        };
                        let thunk = Thunk::new_native(move || {
                            let mut merged = immediate;
                            let flake_result =
                                evaluate_flake_ctx(&dir, Some(child_ctx.clone()))?;
                            if let Value::Attrs(ref flake_out_attrs) = flake_result {
                                for (k, v) in flake_out_attrs.iter() {
                                    if !merged.contains_key(&k) {
                                        merged.insert(k.clone(), v.clone());
                                    }
                                }
                            }
                            Ok(Value::Attrs(Rc::new(merged)))
                        });
                        resolved_inputs.insert(input_name, Value::Thunk(thunk));
                        continue;
                    }
                }

                resolved_inputs.insert(input_name, Value::Attrs(Rc::new(input_val)));
            }
        }

    // 4b. Fill in stub entries for declared-but-unresolved inputs.
    if let Some(inputs_value) = flake_attrs.get("inputs")
        && let Ok(inputs_forced) = crate::eval::force_value(inputs_value)
            && let Value::Attrs(declared_inputs) = inputs_forced {
                for key in declared_inputs.keys() {
                    if !resolved_inputs.contains_key(&key) {
                        // An input reaches here ONLY when it was declared in
                        // `flake.nix` but never resolved from the lock — either
                        // its edge target is missing from `lock.nodes` (§4's
                        // `continue`) or `node_input_edges` could not resolve
                        // the ref at all (it pushes only `if let Ok(target)`,
                        // and `adjacency_map`'s doc concedes "any unresolvable
                        // edges are silently skipped").
                        //
                        // This used to hand the input a FABRICATED store path,
                        // `/nix/store/flake-input-<key>`. That path does not
                        // exist and never will, so the failure surfaced
                        // thousands of eval-steps later as a bare ENOENT on
                        // `import`, naming neither the flake nor the input —
                        // measured on rio 2026-08-08 (`theory/BALIZA-PLAN.md`
                        // §2.5). Worse, the fake path STARTS WITH `/nix/store/`,
                        // so it also picked up copy-to-store string context and
                        // an input-source registration, i.e. it masqueraded as a
                        // real store reference all the way down.
                        //
                        // A resolution miss now fails AT THE BORDER with a typed
                        // error naming `(node, input)`. It stays LAZY — CppNix
                        // only errors when an unresolved input is actually used,
                        // and a flake may legally declare an input it never
                        // touches, so erroring eagerly here would reject flakes
                        // cppnix accepts.
                        let mut stub = NixAttrs::new();
                        let input_key = key.clone();
                        let owner = current_node.clone();
                        stub.insert(
                            "outPath".to_string(),
                            Value::Thunk(crate::value::Thunk::new_native(move || {
                                Err(EvalError::TypeError(format!(
                                    "flake input '{input_key}' is declared in the \
                                     `inputs` of flake node '{owner}' but was not \
                                     resolved from flake.lock: its lock edge is \
                                     missing or unresolvable. sui previously \
                                     substituted the non-existent path \
                                     `/nix/store/flake-input-{input_key}` here, \
                                     which failed later as an opaque ENOENT."
                                )))
                            })),
                        );
                        resolved_inputs.insert(key.clone(), Value::Attrs(Rc::new(stub)));
                    }
                }
            }

    // 4c. Hash the source tree.  Computes the CppNix-compatible
    //     /nix/store/<hash>-source path + SRI narHash that flake
    //     consumers see under `outPath` / `sourceInfo.narHash`.
    //     Verified byte-identical to CppNix on both trivial fixtures
    //     and real pleme-io flakes with .git present.
    let source_hash = sui_compat::source::nar_hash_source_tree(
        std::path::Path::new(&self_path),
        "source",
    ).map_err(|e| EvalError::TypeError(
        format!("getFlake: nar-hashing source tree at {self_path}: {e}")
    ))?;
    let source_store_path = source_hash.store_path.clone();
    let source_nar_sri = source_hash.nar_hash_sri.clone();
    // Byte-parity root (marquee darwin, GATE 1 2026-07-15): a LOCKED flake
    // input's `self` must carry the input's own `rev`/`shortRev`/`lastModified`
    // — exactly as CppNix populates `sourceInfo` for a fetched git input.
    // nix-darwin's `flake.nix` derives the system label from
    // `self.shortRev or self.dirtyShortRev or "dirty"` (→ `darwin-system-25.11.<shortRev>`);
    // without the input's own self-rev, sui fell to `"dirty"` and the cid
    // toplevel drvPath diverged only in that one label string. `current_node`
    // is this flake's node in the ROOT lock (transitive ctx → the locked input;
    // ROOT ctx → the dirty local tree, which carries no `rev` and stays
    // `dirty`-capable, matching CppNix's dirty top-level self).
    let (self_rev, self_last_modified): (Option<String>, Option<i64>) = lock
        .as_ref()
        .and_then(|l| l.nodes.get(&current_node))
        .and_then(|n| n.locked.as_ref())
        .map(|lk| (lk.rev.clone(), lk.last_modified.map(|m| m as i64)))
        .unwrap_or((None, None));
    let self_short_rev: Option<String> =
        self_rev.as_ref().map(|r| r.chars().take(7).collect());

    // `self.outPath` (and `self.sourceInfo.outPath`) is a
    // `/nix/store/<narhash>-source` store reference — the flake's OWN source
    // tree copied into the store. It MUST carry copy-to-store STRING CONTEXT so
    // that, when a downstream derivation embeds it as `src` (the substrate rust
    // builder's `src = self`/`./.` workspace source — `rust_sui`'s whole tree),
    // the dependent's ATerm records the matching `source` inputSrc. cppnix
    // records exactly this; the parity-bisect on the cid darwin toplevel flagged
    // its absence as the `rust_sui` `nix-only=["source"]` inputSrc gap. This is
    // the `self` sibling of the input-outPath context fix above (the input
    // branch attaches this context to each resolved input's `outPath`; here we
    // do the same for the flake's own `self`). Non-store `self_path` (a dirty
    // local tree whose source-hash is still a `-source` store path) also carries
    // the context — `source_store_path` is always a `/nix/store/<h>-source` here.
    let self_out_path_val = {
        let mut ctx = crate::value::StringContext::new();
        ctx.add_plain(source_store_path.as_str());
        Value::String(std::rc::Rc::new(crate::value::NixString::with_context(
            source_store_path.as_str(),
            ctx,
        )))
    };

    let source_info = {
        let mut a = NixAttrs::new();
        a.insert("outPath".to_string(), self_out_path_val.clone());
        a.insert("narHash".to_string(), Value::string(source_nar_sri.clone()));
        if let Some(ref rev) = self_rev {
            a.insert("rev".to_string(), Value::string(rev.clone()));
        }
        if let Some(ref short) = self_short_rev {
            a.insert("shortRev".to_string(), Value::string(short.clone()));
        }
        if let Some(lm) = self_last_modified {
            a.insert("lastModified".to_string(), Value::Int(lm));
        }
        a
    };

    // 5. Build `self` as a CppNix-equivalent fixpoint reference.
    //
    // Critical: `self` must point at the FINAL flake result
    // including outputs (lib, darwinModules, packages, ...) — not
    // just flake-body metadata.  CppNix achieves this via a
    // self-fixpoint: lambdas in outputs body capture `self`, then
    // access e.g. `self.lib.evalConfig` at invocation time AFTER
    // outputs has already returned.
    //
    // We implement this with a shared `OnceCell<Rc<NixAttrs>>`:
    //   - At outputs-call time, `self` is a thunk that reads the
    //     OnceCell (initially empty).
    //   - After outputs returns, we fill the OnceCell with the
    //     final merged attrset.
    //   - Later, when a captured `self` is forced, the OnceCell is
    //     populated and the thunk yields the final attrset.
    //
    // This was the load-bearing M2.1 bug: previously sui passed
    // outputs a `self` containing only outPath/sourceInfo/inputs/
    // flake-body metadata, so `nix-darwin`'s `darwinSystem` body
    // (`self.lib.evalConfig (...)`) errored "attribute not found:
    // 'lib'" at invocation time.
    let self_promise: Rc<std::cell::OnceCell<Rc<NixAttrs>>> = Rc::new(std::cell::OnceCell::new());
    let self_thunk = {
        let self_promise = self_promise.clone();
        // Carry the same store-path context on the fallback skeleton's
        // `outPath` (see `self_out_path_val` above) so a `src = self`
        // coerced in the rare pre-output path still records its `source`
        // inputSrc.
        let fallback_out_path = self_out_path_val.clone();
        Thunk::new_native(move || {
            if let Some(attrs) = self_promise.get() {
                Ok(Value::Attrs(attrs.clone()))
            } else {
                // outputs body forced `self` BEFORE we filled the
                // OnceCell.  This is rare but possible if outputs
                // does something like `forAllSystems = self.…`
                // eagerly at the top level.  Return the pre-output
                // skeleton with whatever flake-body metadata we
                // have so the caller can read outPath etc.
                let mut fallback = NixAttrs::new();
                fallback.insert("outPath".to_string(),
                    fallback_out_path.clone());
                Ok(Value::Attrs(Rc::new(fallback)))
            }
        })
    };

    // 6. Build arguments for `outputs`.
    let mut outputs_args = NixAttrs::new();
    outputs_args.insert("self".to_string(), Value::Thunk(self_thunk));
    for (k, v) in resolved_inputs.iter() {
        outputs_args.insert(k.clone(), v.clone());
    }

    // 7. Call outputs(args).
    let result = crate::eval::apply(outputs_fn, Value::Attrs(Rc::new(outputs_args)))?;
    let result = crate::eval::force_value(&result)?;

    // 8. Build the final flake value.
    //
    // Shape policy lives in `sui-spec/specs/flake.lisp` as a
    // `(defflake-shape :name "cppnix" …)` form.  We consult the
    // spec for the type marker, the spread-outputs rule, and the
    // never-leak denylist — so changes to CppNix's flake shape are
    // one-line Lisp edits, not Rust surgery.  (Previously this
    // function was the drift surface for leak bugs like the
    // `description`-at-top-level regression.)
    let shape = sui_spec::flake::load_canonical().map_err(|e| {
        EvalError::TypeError(format!("flake shape spec failed to load: {e}"))
    })?;
    let mut final_attrs = NixAttrs::new();
    final_attrs.insert("_type".to_string(), Value::string(shape.type_marker.clone()));
    final_attrs.insert("outPath".to_string(), self_out_path_val.clone());
    final_attrs.insert("sourceInfo".to_string(), Value::Attrs(Rc::new(source_info)));
    final_attrs.insert("narHash".to_string(), Value::string(source_nar_sri));
    // Self-rev at the TOP LEVEL of `self` (not only under `sourceInfo`):
    // nix-darwin reads `self.shortRev` / `self.rev` directly. See the
    // source_info block above for the byte-parity rationale.
    if let Some(ref rev) = self_rev {
        final_attrs.insert("rev".to_string(), Value::string(rev.clone()));
    }
    if let Some(ref short) = self_short_rev {
        final_attrs.insert("shortRev".to_string(), Value::string(short.clone()));
    }
    if let Some(lm) = self_last_modified {
        final_attrs.insert("lastModified".to_string(), Value::Int(lm));
    }
    final_attrs.insert("inputs".to_string(), Value::Attrs(Rc::new(resolved_inputs)));
    final_attrs.insert("outputs".to_string(), result.clone());

    if shape.spreads_output_fn() {
        if let Value::Attrs(out_attrs) = &result {
            for (k, v) in out_attrs.iter() {
                if !final_attrs.contains_key(k.as_str()) {
                    final_attrs.insert(k.clone(), v.clone());
                }
            }
        }
    }

    // Also surface flake-body metadata (description, nixConfig) on
    // self.  CppNix exposes these on the flake result, so lambdas
    // captured during outputs that read e.g. `self.description`
    // should resolve correctly.
    for (k, v) in flake_attrs.iter() {
        if k != "outputs" && k != "inputs" && !final_attrs.contains_key(k.as_str()) {
            final_attrs.insert(k.clone(), v.clone());
        }
    }

    let final_attrs_rc = Rc::new(final_attrs);

    // Fill the self-fixpoint promise.  Lambdas captured during
    // outputs now resolve `self.lib`, `self.darwinModules`, etc.
    // against this final attrset.
    let _ = self_promise.set(final_attrs_rc.clone());

    Ok(Value::Attrs(final_attrs_rc))
}

// ── Cached attribute evaluation ──────────────────────────────

/// Evaluate a flake and navigate to a specific attribute, with caching.
///
/// If the derivation path for `(lock_hash, source_hash, attr_path)` is already
/// in the drv cache, returns a synthetic derivation attrset without evaluating
/// the flake (near-zero memory). Otherwise, evaluates normally and caches the
/// result for future lookups.
pub fn evaluate_flake_attr(
    flake_dir: &std::path::Path,
    attr_path: &[&str],
) -> Result<Value, EvalError> {
    let lock_path = flake_dir.join("flake.lock");
    let source_path = flake_dir.join("flake.nix");

    // Compute cache keys from file content.
    let lock_hash = std::fs::read(&lock_path)
        .ok()
        .map(|c| crate::drv_cache::DrvCache::hash_bytes(&c));
    let source_hash = std::fs::read(&source_path)
        .ok()
        .map(|c| crate::drv_cache::DrvCache::hash_bytes(&c));
    let attr_key = attr_path.join(".");

    // Check drv cache.
    if let (Some(lh), Some(sh)) = (&lock_hash, &source_hash) {
        if let Some(entry) = crate::drv_cache::with_cache(|cache| cache.get(lh, sh, &attr_key)) {
            tracing::info!(
                attr_path = %attr_key,
                out_path = %entry.out_path,
                "drv cache hit — skipping full evaluation"
            );
            return Ok(synthetic_drv_value(&entry));
        }
    }

    // Cache miss — full evaluation.
    tracing::info!(attr_path = %attr_key, "drv cache miss — evaluating flake");
    let flake_result = evaluate_flake(flake_dir)?;

    // Navigate to the target attribute.
    let target = navigate_attr_path(&flake_result, attr_path)?;

    // If the result is a derivation, cache it.
    if let (Some(lh), Some(sh)) = (&lock_hash, &source_hash) {
        if let Value::Attrs(ref attrs) = target {
            let drv_path = attrs.get("drvPath").and_then(|v| v.as_string().ok());
            let out_path = attrs.get("outPath").and_then(|v| v.as_string().ok());
            if let (Some(dp), Some(op)) = (drv_path, out_path) {
                crate::drv_cache::with_cache_mut(|cache| {
                    let entry = crate::drv_cache::DrvCacheEntry {
                        drv_path: dp.to_string(),
                        out_path: op.to_string(),
                    };
                    if let Err(e) = cache.put(lh, sh, &attr_key, &entry) {
                        tracing::warn!(error = %e, "Failed to cache derivation path");
                    } else {
                        tracing::info!(attr_path = %attr_key, out_path = %op, "Cached derivation path");
                    }
                });
            }
        }
    }

    Ok(target)
}

/// Navigate an attribute path like `["packages", "x86_64-linux", "default"]`
/// through a Value, forcing thunks at each level.
fn navigate_attr_path(value: &Value, path: &[&str]) -> Result<Value, EvalError> {
    let mut current = crate::eval::force_value(value)?;
    for segment in path {
        let attrs = current.as_attrs().map_err(|_| {
            EvalError::TypeError(format!(
                "expected attrset at '.{segment}', got {}",
                current.type_name()
            ))
        })?;
        let next = attrs.get(*segment).ok_or_else(|| {
            EvalError::AttrNotFound((*segment).to_string())
        })?;
        current = crate::eval::force_value(next)?;
    }
    Ok(current)
}

/// Build a synthetic derivation Value from cached paths.
/// The caller only needs `drvPath`, `outPath`, and `type = "derivation"`.
fn synthetic_drv_value(entry: &crate::drv_cache::DrvCacheEntry) -> Value {
    let mut attrs = NixAttrs::new();
    attrs.insert("type".to_string(), Value::string("derivation"));
    attrs.insert("drvPath".to_string(), Value::string(entry.drv_path.clone()));
    attrs.insert("outPath".to_string(), Value::string(entry.out_path.clone()));
    // Extract name from store path: /nix/store/hash-name → name
    if let Some(name) = entry.out_path.rsplit('/').next().and_then(|b| b.split_once('-').map(|(_, n)| n)) {
        attrs.insert("name".to_string(), Value::string(name));
    }
    Value::Attrs(Rc::new(attrs))
}
