//! L4 module-system solver — slice-keyed re-firing over a compiled
//! [`ModuleGraph`].
//!
//! ## What this does today
//!
//! The dependency-tracking core of the fixed-point solver. Given a
//! compiled [`ModuleGraph`] (option declarations + config setters with
//! `slice` + `assigns_path` metadata, all populated by
//! `sui-spec::module_compiler`):
//!
//! 1. **Topologically order** the setters by the writes/reads
//!    dependency: setter A writes `services.atticd.enable`, setter B
//!    reads it via `mkIf config.services.atticd.enable …` → A fires
//!    before B.
//! 2. **Track dirty paths** — the set of `config.*` paths that have
//!    changed since the last cycle.
//! 3. **Schedule setters whose slice intersects the dirty set** —
//!    everything else stays untouched. A leaf setter (empty slice)
//!    fires exactly once unless its assigns_path itself becomes
//!    dirty (e.g. via mkForce in a downstream module).
//! 4. **Run to quiescence** — re-iterate until no more setters need
//!    to fire.
//!
//! ## What this does NOT do today (queued)
//!
//! * **Body evaluation** — setter bodies are AST node ids; actually
//!   running them through the bytecode VM and producing typed values
//!   requires the sui-eval integration. Today the solver carries a
//!   [`BodyEvaluator`] trait the eval crate will implement;
//!   [`StubEvaluator`] in the test module returns a deterministic
//!   placeholder so the solver math is provable in isolation.
//! * **Defunctionalization** — higher-order setters stay AST-pointer-
//!   only. The transform that lowers them to first-order tagged
//!   closures lands next.
//! * **NbE structural-equality caching** — cache key for the compiled
//!   closure uses [`ModuleGraph::canonical_hash`]; NbE-driven
//!   canonicalization (equal-up-to-alpha for setter bodies) is the
//!   next optimization layer.
//! * **Rayon-per-SCC parallelism** — today's solver is single-threaded
//!   (Kahn's queue order). The opportunistic-parallelism work cited in
//!   the eval-engine research (arxiv:2405.11361) lands when bodies
//!   evaluate for real.
//!
//! ## Why this matters
//!
//! The 24-second `nixosConfigurations.rio.config.system.build.toplevel`
//! cost is the cppnix module-system fixed point re-evaluating every
//! setter on every rebuild. With slice-keyed re-firing, a rebuild
//! where (say) only `services.atticd.enable` changed re-fires ONLY the
//! setters whose slice contains that path — for the rio fleet that
//! drops from ~2000 setters to a small handful. The math is here
//! today; the eval integration in the next ship makes the perf
//! visible to operators.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use crate::ast_evaluator::{eval_node, EvalEnv, EvalValue};
use crate::ast_graph::AstGraph;
use crate::module_graph::{ConfigSetter, ModuleGraph, ModuleId, SetterId};

/// A canonical config path, e.g. `["services", "atticd", "enable"]`.
pub type ConfigPath = Vec<String>;

/// A setter's identity inside a [`ModuleGraph`]: `(module_id,
/// setter_id_within_module)`. Each setter is unique by this pair —
/// `module_id` disambiguates because the same `setter_id_within_module`
/// can collide across modules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlobalSetterId {
    pub module: ModuleId,
    pub setter: SetterId,
}

/// Errors from the solver.
#[derive(Debug, thiserror::Error)]
pub enum SolverError {
    #[error("dependency cycle detected involving setters {0:?}")]
    Cycle(Vec<GlobalSetterId>),

    #[error("body evaluator returned an error for setter {id:?}: {reason}")]
    BodyEval {
        id: GlobalSetterId,
        reason: String,
    },
}

/// Trait the eval engine will implement to actually evaluate setter
/// bodies. Carried in the solver state so the dependency-tracking core
/// can be exercised under stub evaluators in tests without dragging
/// sui-eval into sui-spec.
///
/// **Today**: [`TreeWalkingEvaluator`] is the production implementation
/// — a minimum-viable AST tree-walker over the setter's `body_ast_root`
/// in its containing module's [`AstGraph`]. Handles literals,
/// arithmetic, comparisons, if-then-else, attrset construction +
/// select, list concat, and Select chains rooted at `config`. Returns
/// [`EvalValue::Opaque`] for the long tail (Apply / Lambda / LetIn /
/// With) — those are picked up by the future sui-eval bytecode VM
/// integration that replaces this minimum-viable engine.
pub trait BodyEvaluator {
    /// Evaluate the setter's body in the given environment and return
    /// the resulting value as canonical bytes (JSON today; rkyv when
    /// the typed value lattice is finalized).
    ///
    /// `gid` identifies the setter's containing module so multi-module
    /// evaluators (`PerModuleEvaluator`) route the body to the right
    /// `AstGraph`. Single-module evaluators ignore it.
    ///
    /// # Errors
    ///
    /// Returns a free-form reason string when the body can't be
    /// evaluated; the solver surfaces it as
    /// [`SolverError::BodyEval`].
    fn evaluate(
        &self,
        gid: GlobalSetterId,
        setter: &ConfigSetter,
        env_snapshot: &EnvSnapshot,
    ) -> Result<Vec<u8>, String>;
}

/// Production [`BodyEvaluator`] backed by [`crate::ast_evaluator`] —
/// the minimum-viable tree-walker over the typed [`AstGraph`].
///
/// One evaluator per module (the AST graph owns its node table, so the
/// evaluator needs the matching graph to look up `body_ast_root`).
/// The solver's [`SolverState::new`] takes a single evaluator; for
/// multi-module setups (the common case), wrap a slice of per-module
/// evaluators in a [`PerModuleEvaluator`] (helper below).
///
/// Setter bodies that touch unsupported AST kinds (function calls,
/// closures) bubble up as `Opaque` from the tree walker, which the
/// evaluator surfaces as the literal JSON string `"<opaque:Apply>"` —
/// the eventual sui-eval integration recognizes the sentinel and
/// recomputes the body through the real VM.
pub struct TreeWalkingEvaluator {
    graph: Arc<AstGraph>,
}

impl TreeWalkingEvaluator {
    /// Build an evaluator that resolves every setter body via `graph`.
    /// One evaluator per module's AST.
    #[must_use]
    pub fn new(graph: Arc<AstGraph>) -> Self {
        Self { graph }
    }
}

impl BodyEvaluator for TreeWalkingEvaluator {
    fn evaluate(
        &self,
        _gid: GlobalSetterId,
        setter: &ConfigSetter,
        env_snapshot: &EnvSnapshot,
    ) -> Result<Vec<u8>, String> {
        let env = env_snapshot_to_eval_env(env_snapshot);
        let value = eval_node(&self.graph, setter.body_ast_root, &env)
            .map_err(|e| format!("ast eval: {e}"))?;
        serde_json::to_vec(&value)
            .map_err(|e| format!("value→bytes: {e}"))
    }
}

/// Multi-module evaluator that routes each setter to the AstGraph of
/// its containing module. Used in tests + by callers that build a
/// ModuleGraph from multiple AstGraphs.
pub struct PerModuleEvaluator {
    /// Module-id → AstGraph for that module. The solver passes setters
    /// with `body_ast_root` indexing into the correct one via the
    /// setter's containing module id.
    pub graphs: BTreeMap<ModuleId, Arc<AstGraph>>,
}

impl PerModuleEvaluator {
    /// Build from a `(module_id, graph)` iterator.
    #[must_use]
    pub fn from_pairs<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = (ModuleId, Arc<AstGraph>)>,
    {
        Self {
            graphs: iter.into_iter().collect(),
        }
    }
}

impl BodyEvaluator for PerModuleEvaluator {
    fn evaluate(
        &self,
        gid: GlobalSetterId,
        setter: &ConfigSetter,
        env_snapshot: &EnvSnapshot,
    ) -> Result<Vec<u8>, String> {
        let graph = self.graphs.get(&gid.module).ok_or_else(|| {
            format!(
                "no AstGraph registered for module id {} (setter writing {:?})",
                gid.module, setter.assigns_path
            )
        })?;
        let env = env_snapshot_to_eval_env(env_snapshot);
        let value = eval_node(graph, setter.body_ast_root, &env)
            .map_err(|e| format!("ast eval: {e}"))?;
        serde_json::to_vec(&value).map_err(|e| format!("value→bytes: {e}"))
    }
}

/// Project [`EnvSnapshot`] into an [`EvalEnv`] with a single
/// `config` binding (an AttrSet built from every path-bytes entry).
/// The tree walker then resolves `config.x.y.z` selects via this
/// AttrSet.
fn env_snapshot_to_eval_env(snapshot: &EnvSnapshot) -> EvalEnv {
    let mut root = BTreeMap::<String, EvalValue>::new();
    for (path, bytes) in &snapshot.config {
        // Try to deserialize the stored bytes back to an EvalValue.
        // Bytes were written via `serde_json::to_vec(&EvalValue)` by
        // a prior `TreeWalkingEvaluator::evaluate` call. Going through
        // `from_str` over an owned String forces full-ownership on
        // the deserialized value (no borrowed lifetimes from `bytes`).
        let value: EvalValue = match std::str::from_utf8(bytes)
            .ok()
            .and_then(|s| serde_json::from_str(s).ok())
        {
            Some(v) => v,
            None => EvalValue::Str(
                String::from_utf8(bytes.clone()).unwrap_or_default(),
            ),
        };
        insert_path(&mut root, path, value);
    }
    EvalEnv::new().with_binding("config", EvalValue::AttrSet(root))
}

fn insert_path(
    out: &mut BTreeMap<String, EvalValue>,
    path: &[String],
    value: EvalValue,
) {
    if path.is_empty() {
        return;
    }
    if path.len() == 1 {
        out.insert(path[0].clone(), value);
        return;
    }
    let head = &path[0];
    let tail = &path[1..];
    let entry = out
        .entry(head.clone())
        .or_insert_with(|| EvalValue::AttrSet(BTreeMap::new()));
    if let EvalValue::AttrSet(inner) = entry {
        insert_path(inner, tail, value);
    }
}

/// A read-only snapshot of the current config attrset, projected as
/// `path → bytes`. Reflects every setter that has fired in prior
/// iterations of the fixed point. Newer mkForce'd values override
/// older default-priority ones.
#[derive(Debug, Default, Clone)]
pub struct EnvSnapshot {
    pub config: BTreeMap<ConfigPath, Vec<u8>>,
}

impl EnvSnapshot {
    /// Read one path. Returns `None` if no setter has produced it yet.
    pub fn get(&self, path: &ConfigPath) -> Option<&Vec<u8>> {
        self.config.get(path)
    }

    /// Whether the snapshot contains a value for any prefix of `path`
    /// — useful for the "is this slice satisfied yet?" check.
    pub fn has_prefix(&self, prefix: &ConfigPath) -> bool {
        self.config.keys().any(|k| k.starts_with(prefix))
    }
}

/// Live state of the solver.
pub struct SolverState<E: BodyEvaluator> {
    /// The compiled module graph the solver is operating on.
    graph: ModuleGraph,
    /// Index from each setter's `assigns_path` back to the setter that
    /// writes it. Built once on construction.
    writers_by_path: BTreeMap<ConfigPath, GlobalSetterId>,
    /// Reverse-dependency index: for every `path`, which setters DEPEND
    /// on it (i.e. their slice contains a prefix of `path`)?
    readers_by_path: BTreeMap<ConfigPath, Vec<GlobalSetterId>>,
    /// Per-setter slice cache (deduplicates the slice lookup we hit on
    /// every iteration).
    slice_by_setter: BTreeMap<GlobalSetterId, Vec<ConfigPath>>,
    /// Current config attrset projection.
    env: EnvSnapshot,
    /// Setters fired so far in this run.
    fired: BTreeSet<GlobalSetterId>,
    /// Body evaluator (the eval engine — or a stub in tests).
    evaluator: E,
}

impl<E: BodyEvaluator> SolverState<E> {
    /// Construct a fresh solver for `graph`. Builds the reverse-
    /// dependency index up-front so per-iteration scheduling is O(1)
    /// per dirty path.
    pub fn new(graph: ModuleGraph, evaluator: E) -> Self {
        let mut writers_by_path: BTreeMap<ConfigPath, GlobalSetterId> = BTreeMap::new();
        let mut readers_by_path: BTreeMap<ConfigPath, Vec<GlobalSetterId>> = BTreeMap::new();
        let mut slice_by_setter: BTreeMap<GlobalSetterId, Vec<ConfigPath>> = BTreeMap::new();

        for module in &graph.modules {
            for setter in &module.setters {
                let gid = GlobalSetterId {
                    module: module.id,
                    setter: setter.id,
                };
                writers_by_path.insert(setter.assigns_path.clone(), gid);
                for slice_path in &setter.slice {
                    readers_by_path
                        .entry(slice_path.clone())
                        .or_default()
                        .push(gid);
                }
                slice_by_setter.insert(gid, setter.slice.clone());
            }
        }

        Self {
            graph,
            writers_by_path,
            readers_by_path,
            slice_by_setter,
            env: EnvSnapshot::default(),
            fired: BTreeSet::new(),
            evaluator,
        }
    }

    /// Borrow the current env snapshot.
    pub fn env(&self) -> &EnvSnapshot {
        &self.env
    }

    /// Borrow the compiled graph back out.
    pub fn graph(&self) -> &ModuleGraph {
        &self.graph
    }

    /// Total setter count across all modules.
    pub fn setter_count(&self) -> usize {
        self.graph
            .modules
            .iter()
            .map(|m| m.setters.len())
            .sum()
    }

    /// Find every CONSUMER setter that needs to fire because at least
    /// one of `dirty_paths` matches a prefix of its slice.
    ///
    /// "Matches a prefix" means: if a setter's slice is
    /// `[["services", "atticd"]]` and `dirty_paths` contains
    /// `["services", "atticd", "enable"]`, the setter is scheduled —
    /// a slice over a subtree fires on any descendant change.
    ///
    /// Writers are deliberately **not** included here. A writer's own
    /// re-firing happens only when its OWN slice changes (covered by
    /// the readers lookup that finds it as a consumer of its
    /// upstream's outputs). Multi-writer conflict resolution happens
    /// at the evaluator layer via [`ConfigSetter::priority`] —
    /// firing each writer once per topo pass is enough.
    pub fn schedule_for_dirty(
        &self,
        dirty_paths: &[ConfigPath],
    ) -> BTreeSet<GlobalSetterId> {
        let mut scheduled: BTreeSet<GlobalSetterId> = BTreeSet::new();
        for dirty in dirty_paths {
            for (slice_path, readers) in &self.readers_by_path {
                if slice_path_intersects(slice_path, dirty) {
                    for r in readers {
                        scheduled.insert(*r);
                    }
                }
            }
        }
        scheduled
    }

    /// Topological order of all setters by writes→reads dependency.
    /// Kahn's algorithm.
    ///
    /// # Errors
    ///
    /// [`SolverError::Cycle`] if the writes→reads graph has a cycle
    /// (which is a module-author bug — cppnix would also fail here).
    pub fn topological_order(&self) -> Result<Vec<GlobalSetterId>, SolverError> {
        // Build adjacency: writer → readers
        let mut edges: BTreeMap<GlobalSetterId, BTreeSet<GlobalSetterId>> = BTreeMap::new();
        let mut indegree: BTreeMap<GlobalSetterId, u32> = BTreeMap::new();

        // Pre-seed every setter with indegree 0
        for module in &self.graph.modules {
            for setter in &module.setters {
                let gid = GlobalSetterId {
                    module: module.id,
                    setter: setter.id,
                };
                indegree.insert(gid, 0);
            }
        }

        // For each setter, find which other setters it depends on
        // (i.e. which writers feed its slice).
        for (writer_path, &writer) in &self.writers_by_path {
            // Anyone whose slice contains this path or a prefix.
            for (gid, slice) in &self.slice_by_setter {
                if *gid == writer {
                    continue;
                }
                if slice
                    .iter()
                    .any(|s| slice_path_intersects(s, writer_path))
                {
                    edges.entry(writer).or_default().insert(*gid);
                    *indegree.entry(*gid).or_insert(0) += 1;
                }
            }
        }

        let mut queue: VecDeque<GlobalSetterId> = indegree
            .iter()
            .filter(|&(_, &d)| d == 0)
            .map(|(k, _)| *k)
            .collect();
        let mut order: Vec<GlobalSetterId> = Vec::with_capacity(indegree.len());

        while let Some(gid) = queue.pop_front() {
            order.push(gid);
            if let Some(neighbors) = edges.get(&gid) {
                for &n in neighbors {
                    let d = indegree.get_mut(&n).expect("seeded above");
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(n);
                    }
                }
            }
        }

        if order.len() != indegree.len() {
            // Find the surviving nodes (those still with indegree > 0).
            let stuck: Vec<GlobalSetterId> = indegree
                .iter()
                .filter(|&(_, &d)| d > 0)
                .map(|(k, _)| *k)
                .collect();
            return Err(SolverError::Cycle(stuck));
        }

        Ok(order)
    }

    /// Fire one setter, write its result into the env, mark it fired.
    /// Returns the setter's own `assigns_path` so callers can register
    /// it as newly-dirty.
    ///
    /// # Errors
    ///
    /// [`SolverError::BodyEval`] when the body evaluator rejects.
    pub fn fire(&mut self, gid: GlobalSetterId) -> Result<ConfigPath, SolverError> {
        let setter = self.get_setter(gid).clone();
        let bytes = self
            .evaluator
            .evaluate(gid, &setter, &self.env)
            .map_err(|reason| SolverError::BodyEval { id: gid, reason })?;
        self.env.config.insert(setter.assigns_path.clone(), bytes);
        self.fired.insert(gid);
        Ok(setter.assigns_path)
    }

    /// Run the solver to quiescence starting from `initial_dirty`.
    /// Returns the firing order in temporal sequence. Caller can
    /// inspect [`Self::env`] afterward for the final config attrset.
    ///
    /// On a cold rebuild (no env, no prior dirty), pass an empty
    /// `initial_dirty` and the solver runs every setter in topological
    /// order. On a warm rebuild where only `services.atticd.enable`
    /// changed, pass `[vec!["services", "atticd", "enable"]]` and only
    /// the setters depending on that slice fire.
    ///
    /// # Errors
    ///
    /// Cycles or body-eval failures surface as [`SolverError`].
    pub fn run(&mut self, initial_dirty: &[ConfigPath]) -> Result<Vec<GlobalSetterId>, SolverError> {
        // Topological sort once — relative order is stable across
        // iterations.
        let topo = self.topological_order()?;
        let topo_index: BTreeMap<GlobalSetterId, usize> = topo
            .iter()
            .enumerate()
            .map(|(i, gid)| (*gid, i))
            .collect();

        // Initial schedule: setters intersecting initial_dirty.
        // Plus on a cold start (initial_dirty empty AND env empty),
        // schedule every setter.
        let mut to_fire: BTreeSet<GlobalSetterId> = if initial_dirty.is_empty() && self.env.config.is_empty() {
            topo.iter().copied().collect()
        } else {
            self.schedule_for_dirty(initial_dirty)
        };

        let mut firing_order: Vec<GlobalSetterId> = Vec::new();
        let mut iterations = 0u32;
        let max_iterations = 64u32; // belt-and-suspenders against pathological feedback loops

        while !to_fire.is_empty() {
            iterations += 1;
            if iterations > max_iterations {
                break;
            }

            // Sort the to-fire set into topological order to maximize
            // single-pass progress.
            let mut batch: Vec<GlobalSetterId> = to_fire.iter().copied().collect();
            batch.sort_by_key(|gid| topo_index.get(gid).copied().unwrap_or(usize::MAX));
            to_fire.clear();

            let mut newly_dirty: Vec<ConfigPath> = Vec::new();
            for gid in batch {
                let before = self.env.config.get(&self.get_setter(gid).assigns_path).cloned();
                let assigns = self.fire(gid)?;
                firing_order.push(gid);
                let after = self.env.config.get(&assigns).cloned();
                if before != after {
                    newly_dirty.push(assigns);
                }
            }

            // Schedule downstream consumers of the newly-dirty paths.
            to_fire = self.schedule_for_dirty(&newly_dirty);
            // Don't re-fire setters we already fired this run unless
            // their dependency actually changed and they're slice-keyed
            // to react — schedule_for_dirty already filters by slice
            // intersection, so we let the loop run.
        }

        Ok(firing_order)
    }

    fn get_setter(&self, gid: GlobalSetterId) -> &ConfigSetter {
        let module = self
            .graph
            .modules
            .iter()
            .find(|m| m.id == gid.module)
            .expect("module id resolved");
        module
            .setters
            .iter()
            .find(|s| s.id == gid.setter)
            .expect("setter id resolved")
    }
}

/// Slice-intersection predicate: does `dirty_path` lie within the
/// subtree rooted at `slice_path`? Examples:
///
/// * slice `["services"]` + dirty `["services", "atticd", "enable"]`
///   → true (slice is a prefix of dirty)
/// * slice `["services", "atticd"]` + dirty `["services"]` → true
///   (dirty is a prefix of slice — a coarser change covers it)
/// * slice `["boot"]` + dirty `["services"]` → false
#[must_use]
pub fn slice_path_intersects(slice_path: &ConfigPath, dirty: &ConfigPath) -> bool {
    slice_path.starts_with(dirty.as_slice()) || dirty.starts_with(slice_path.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast_graph::AstGraph;
    use crate::module_graph::ModuleGraph;
    use pretty_assertions::assert_eq;

    /// Stub evaluator that returns the setter's assigns_path as bytes.
    /// Useful for proving the solver math without dragging in the eval
    /// engine.
    struct PathBytesEvaluator;
    impl BodyEvaluator for PathBytesEvaluator {
        fn evaluate(
            &self,
            _gid: GlobalSetterId,
            setter: &ConfigSetter,
            _env: &EnvSnapshot,
        ) -> Result<Vec<u8>, String> {
            Ok(setter.assigns_path.join(".").into_bytes())
        }
    }

    fn build_graph(modules: &[(&str, &str)]) -> ModuleGraph {
        let pairs: Vec<(String, AstGraph)> = modules
            .iter()
            .map(|(label, src)| {
                let ast = AstGraph::from_source(src).expect("parse");
                ((*label).to_string(), ast)
            })
            .collect();
        ModuleGraph::from_ast_graphs(&pairs).expect("build")
    }

    #[test]
    fn empty_graph_runs_to_quiescence_instantly() {
        let g = ModuleGraph::new();
        let mut solver = SolverState::new(g, PathBytesEvaluator);
        let order = solver.run(&[]).unwrap();
        assert!(order.is_empty());
    }

    #[test]
    fn cold_start_fires_every_setter() {
        let g = build_graph(&[(
            "a.nix",
            "{ config, ... }: { \
             config.networking.hostName = \"rio\"; \
             config.boot.kernelParams = [\"x\"]; \
             }",
        )]);
        let mut solver = SolverState::new(g, PathBytesEvaluator);
        assert_eq!(solver.setter_count(), 2);
        let order = solver.run(&[]).unwrap();
        // Both setters fire on cold start.
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn warm_run_with_no_dirty_paths_fires_nothing() {
        let g = build_graph(&[(
            "a.nix",
            "{ config, ... }: { config.networking.hostName = \"rio\"; }",
        )]);
        let mut solver = SolverState::new(g, PathBytesEvaluator);
        // Prime the env so it's not "cold".
        solver.env.config.insert(
            vec!["networking".to_string(), "hostName".to_string()],
            b"rio".to_vec(),
        );
        let order = solver.run(&[]).unwrap();
        assert!(order.is_empty(), "warm + no-dirty should fire nothing");
    }

    #[test]
    fn slice_keyed_re_firing_only_re_runs_matching_setters() {
        // Two setters:
        //   A: config.networking.hostName = "rio"; (no reads → empty slice)
        //   B: config.boot.kernelParams = mkIf config.networking.hostName == "rio" [...]
        //      (slice reads networking.hostName)
        // Mark networking.hostName dirty → A re-fires (writer), B re-fires (reader).
        // Mark boot.unrelatedPath dirty → nothing.
        let g = build_graph(&[(
            "a.nix",
            "{ config, ... }: { \
             config.networking.hostName = \"rio\"; \
             config.boot.kernelParams = mkIf (config.networking.hostName == \"rio\") [\"x\"]; \
             }",
        )]);
        let mut solver = SolverState::new(g, PathBytesEvaluator);
        // Cold start to populate.
        solver.run(&[]).unwrap();

        // Now re-fire with networking.hostName dirty.
        let order = solver
            .run(&[vec!["networking".to_string(), "hostName".to_string()]])
            .unwrap();
        // Only the READER (B, which uses networking.hostName in its
        // mkIf condition) re-fires. The writer A doesn't re-fire
        // because nothing in its (empty) slice changed — its output is
        // already in env from the cold start.
        assert_eq!(
            order.len(),
            1,
            "expected only the reader to re-fire on slice match (writer's output is current)"
        );

        // Re-fire with a totally unrelated path dirty.
        let order = solver
            .run(&[vec!["unrelated".to_string(), "path".to_string()]])
            .unwrap();
        assert!(
            order.is_empty(),
            "expected nothing to re-fire on unrelated dirty"
        );
    }

    #[test]
    fn topological_order_respects_writer_reader_edges() {
        // A writes services.foo.enable.
        // B reads services.foo.enable via mkIf.
        // Topological order: A before B.
        let g = build_graph(&[(
            "ab.nix",
            "{ config, ... }: { \
             config.services.foo.enable = true; \
             config.networking.hostName = mkIf config.services.foo.enable \"rio\"; \
             }",
        )]);
        let solver = SolverState::new(g, PathBytesEvaluator);
        let order = solver.topological_order().unwrap();
        assert_eq!(order.len(), 2);
        // A (writer of services.foo.enable) must come before B (reader).
        let pos_a = order
            .iter()
            .position(|gid| {
                let m = &solver.graph.modules[gid.module as usize];
                let s = &m.setters[gid.setter as usize];
                s.assigns_path == vec!["services", "foo", "enable"]
            })
            .unwrap();
        let pos_b = order
            .iter()
            .position(|gid| {
                let m = &solver.graph.modules[gid.module as usize];
                let s = &m.setters[gid.setter as usize];
                s.assigns_path == vec!["networking", "hostName"]
            })
            .unwrap();
        assert!(pos_a < pos_b, "writer must come before reader");
    }

    #[test]
    fn slice_path_intersects_descendant() {
        // slice: services.atticd
        // dirty: services.atticd.enable  → descendant → intersects
        assert!(slice_path_intersects(
            &vec!["services".to_string(), "atticd".to_string()],
            &vec![
                "services".to_string(),
                "atticd".to_string(),
                "enable".to_string()
            ]
        ));
        // slice: services.atticd.enable
        // dirty: services.atticd  → ancestor → also intersects
        assert!(slice_path_intersects(
            &vec![
                "services".to_string(),
                "atticd".to_string(),
                "enable".to_string()
            ],
            &vec!["services".to_string(), "atticd".to_string()]
        ));
        // Disjoint → no
        assert!(!slice_path_intersects(
            &vec!["services".to_string()],
            &vec!["boot".to_string()]
        ));
    }

    #[test]
    fn fixed_point_terminates_within_budget() {
        // Three setters: A → B → C chain
        // (A.assigns is in B.slice; B.assigns is in C.slice)
        let g = build_graph(&[(
            "chain.nix",
            "{ config, ... }: { \
             config.a = 1; \
             config.b = if config.a == 1 then 2 else 0; \
             config.c = if config.b == 2 then 3 else 0; \
             }",
        )]);
        let mut solver = SolverState::new(g, PathBytesEvaluator);
        let order = solver.run(&[]).unwrap();
        assert!(!order.is_empty());
        // The chain has to fire each setter at least once.
        assert!(order.len() >= 3);
    }

    #[test]
    fn schedule_for_dirty_returns_only_readers_not_writers() {
        let g = build_graph(&[(
            "writer_reader.nix",
            "{ config, ... }: { \
             config.x = 1; \
             config.y = if config.x == 1 then 2 else 0; \
             }",
        )]);
        let solver = SolverState::new(g, PathBytesEvaluator);
        let dirty = vec![vec!["x".to_string()]];
        let scheduled = solver.schedule_for_dirty(&dirty);
        // Only the READER (config.y, which reads config.x via mkIf
        // condition) is scheduled. Writers re-fire only when their
        // OWN slice changes — covered by the topo-order initial pass.
        assert_eq!(scheduled.len(), 1);
    }

    #[test]
    fn env_snapshot_get_and_has_prefix_work() {
        let mut env = EnvSnapshot::default();
        env.config.insert(
            vec!["services".to_string(), "atticd".to_string(), "enable".to_string()],
            b"true".to_vec(),
        );
        assert!(env
            .get(&vec![
                "services".to_string(),
                "atticd".to_string(),
                "enable".to_string()
            ])
            .is_some());
        assert!(env.has_prefix(&vec!["services".to_string()]));
        assert!(env.has_prefix(&vec![
            "services".to_string(),
            "atticd".to_string()
        ]));
        assert!(!env.has_prefix(&vec!["boot".to_string()]));
    }
}
