//! canteiro_spec — the **`(defci)` AUTHORING SURFACE**: a CI run declared as
//! typed Lisp data (theory/CANTEIRO.md §4, leg 2 — "pleme-io/actions built out
//! of reusable Rust and caixa"). This is the TYPED-SPEC + INTERPRETER TRIPLET
//! for authoring a [`crate::canteiro::CiRun`] as a `(defci …)` form:
//!
//! 1. **Typed border** — [`CiSpec`] (+ the per-node [`CiNodeSpec`]) with
//!    `#[derive(DeriveTataraDomain)]` `#[tatara(keyword = "defci")]`, exactly the
//!    pattern [`crate::SuperCacheCiConfig`]'s `defsupercacheci` proves compiles in
//!    this crate. The nested `Vec<CiNodeSpec>` rides the derive's `VecDeserialize`
//!    arm (the sexp→serde-JSON bridge that unlocks `Vec<Struct>`), so the derive
//!    attaches cleanly on this shape — no flattening or stubbing was needed.
//! 2. **Authored form** — the exact shape the keyword accepts:
//!    ```lisp
//!    (defci
//!      :workspace "pleme-io"
//!      :repo      "sui"
//!      :nodes ((:name "build" :command "cargo"
//!               :args ("build" "-p" "sui-supercacheci")
//!               :inputs ("sui-supercacheci/src/"))
//!              (:name "test" :command "cargo"
//!               :args ("test" "-p" "sui-supercacheci" "canteiro")
//!               :deps ("build"))))
//!    ```
//!    Each node is a kwargs plist; `:args`/`:deps`/`:inputs` are string lists
//!    (omittable — they default empty) and `:env` a string (omittable — defaults
//!    `"none"`). `:env` maps to [`EnvClass`]: `"none"` → [`EnvClass::None`],
//!    `"localstack"` → [`EnvClass::LocalStack`], `"warmpool:<ref>"` →
//!    [`EnvClass::WarmPoolClaim`]; any other string is a typed [`CiSpecError`].
//! 3. **Interpreter** — [`CiSpec::to_ci_run`] maps the authored data onto a real
//!    [`CiRun`] (each [`CiNodeSpec`] → [`CiNode`] via `CiNode::new(…).with_inputs(…)`),
//!    so the whole shipped canteiro pipeline (`decompose` / `emit_gha` /
//!    `run_in_process`) drives a `(defci)`-authored run unchanged.
//!
//! ## ON THE RECORD — honest scope (never rounded up)
//!
//! This lands the `(defci)` authoring surface **colocated with [`CiRun`] in
//! `sui-supercacheci`** — reusable Rust + a tatara-lisp `(defci)` form, the
//! authoring half of CANTEIRO leg 2. Promoting it to a first-class **caixa
//! `:kind Acao`** arm in the `pleme-io/caixa` repo (which needs [`CiRun`] to be a
//! shareable type across repos) is the **NAMED DESTINATION**, not done here. The
//! caixa-kind promotion is future work; what ships is the typed border + the
//! `(defci)` keyword + the `to_ci_run` interpreter, all local to this crate.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::canteiro::{ActionRef, CiNode, CiRun, EnvClass};

/// One authored CI node — the Lisp-data mirror of a [`CiNode`], parsed from a
/// kwargs plist inside a `(defci … :nodes (…))` form. It is NOT itself a
/// `TataraDomain`; it rides the parent [`CiSpec`]'s derive as a nested
/// serde-deserialized element (the `Vec<Struct>` arm), so it needs only
/// `Serialize`/`Deserialize`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CiNodeSpec {
    /// The node name (the DAG subject + the [`ActionRef`] display name).
    pub name: String,
    /// The command the node runs.
    pub command: String,
    /// The command's arguments. Omittable in the authored form (defaults empty).
    #[serde(default)]
    pub args: Vec<String>,
    /// Names of the nodes this one depends on (each becomes a DAG edge
    /// `dep → this`). Omittable (defaults empty — a root node).
    #[serde(default)]
    pub deps: Vec<String>,
    /// The repo-relative input prefixes this node consumes (feeds
    /// [`crate::canteiro::affected_set`]). Omittable — an empty inputs list is
    /// the CONSERVATIVE always-affected class (see [`CiNode::inputs`]).
    #[serde(default)]
    pub inputs: Vec<String>,
    /// The environment class as a string: `"none"` / `"localstack"` /
    /// `"warmpool:<ref>"`. Omittable (defaults `"none"`); parsed to [`EnvClass`]
    /// by [`CiSpec::to_ci_run`], with any other value a typed [`CiSpecError`].
    #[serde(default = "env_none")]
    pub env: String,
}

/// The default `env` string — the no-environment class.
fn env_none() -> String {
    "none".to_string()
}

/// The authored CI run — the `(defci …)` typed border. Its `nodes` field is a
/// `Vec<CiNodeSpec>`, parsed by the derive's `VecDeserialize` arm; each element
/// is a kwargs plist. [`to_ci_run`](CiSpec::to_ci_run) is the interpreter half
/// of the triplet.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[tatara(keyword = "defci")]
pub struct CiSpec {
    /// The workspace (the [`CiRun`] scope's workspace).
    pub workspace: String,
    /// The repo (the [`CiRun`] scope's repo).
    pub repo: String,
    /// The declared nodes, each authored as a `(:name … :command … …)` plist.
    pub nodes: Vec<CiNodeSpec>,
}

/// Every way an authored [`CiSpec`] fails to interpret into a [`CiRun`]. A bad
/// `:env` string is a typed rejection, never a silent default or a panic.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CiSpecError {
    /// A node's `env` string is not one of the recognized classes.
    #[error(
        "node `{node}` has an unrecognized env class `{env}` \
         (expected `none`, `localstack`, or `warmpool:<ref>`)"
    )]
    BadEnv { node: String, env: String },
    /// A node declared `warmpool:` with no reference after the colon.
    #[error("node `{node}` declares an empty warmpool reference (`warmpool:` needs a `<ref>`)")]
    EmptyWarmpoolRef { node: String },
}

/// Parse the authored `env` string of node `node` into a typed [`EnvClass`].
fn parse_env(node: &str, env: &str) -> Result<EnvClass, CiSpecError> {
    match env {
        "none" => Ok(EnvClass::None),
        "localstack" => Ok(EnvClass::LocalStack),
        _ => match env.strip_prefix("warmpool:") {
            Some("") => Err(CiSpecError::EmptyWarmpoolRef {
                node: node.to_string(),
            }),
            Some(rest) => Ok(EnvClass::WarmPoolClaim(rest.to_string())),
            None => Err(CiSpecError::BadEnv {
                node: node.to_string(),
                env: env.to_string(),
            }),
        },
    }
}

impl CiNodeSpec {
    /// Interpret this authored node into a real [`CiNode`], mapping its `env`
    /// string to a typed [`EnvClass`] and preserving `deps` + `inputs`.
    ///
    /// # Errors
    /// - [`CiSpecError::BadEnv`] — the `env` string is not a recognized class.
    /// - [`CiSpecError::EmptyWarmpoolRef`] — `warmpool:` with no `<ref>`.
    fn to_ci_node(&self) -> Result<CiNode, CiSpecError> {
        let env_class = parse_env(&self.name, &self.env)?;
        let action = ActionRef {
            name: self.name.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
        };
        Ok(CiNode::new(self.name.clone(), env_class, action, self.deps.clone())
            .with_inputs(self.inputs.clone()))
    }
}

impl CiSpec {
    /// **The interpreter half of the triplet** — map this authored [`CiSpec`]
    /// onto a real [`CiRun`]. Each [`CiNodeSpec`] becomes a [`CiNode`]
    /// (`CiNode::new(…).with_inputs(…)`, its `env` string typed to [`EnvClass`]),
    /// preserving declaration order so the resulting run decomposes to exactly
    /// the authored DAG. The result feeds the shipped canteiro pipeline
    /// (`decompose` / `emit_gha` / `run_in_process`) unchanged.
    ///
    /// # Errors
    /// - [`CiSpecError`] — a node's `env` string is invalid (bad class or an
    ///   empty warmpool ref). The DAG-shape errors (dup / dangling / cycle) are
    ///   surfaced later by [`crate::canteiro::decompose`], not here.
    pub fn to_ci_run(&self) -> Result<CiRun, CiSpecError> {
        let mut nodes = Vec::with_capacity(self.nodes.len());
        for n in &self.nodes {
            nodes.push(n.to_ci_node()?);
        }
        Ok(CiRun {
            workspace: self.workspace.clone(),
            repo: self.repo.clone(),
            nodes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canteiro::decompose;
    use crate::canteiro_gha::{emit_gha, emit_gha_yaml};
    use tatara_lisp::{TataraDomain, compile_typed};

    /// The canonical authored `(defci)` form: a 2-node `build → test` sui run,
    /// with real cargo commands + declared inputs on `build`.
    const DEFCI_BUILD_TEST: &str = r#"
        (defci
          :workspace "pleme-io"
          :repo      "sui"
          :nodes ((:name "build" :command "cargo"
                   :args ("build" "-p" "sui-supercacheci")
                   :inputs ("sui-supercacheci/src/"))
                  (:name "test" :command "cargo"
                   :args ("test" "-p" "sui-supercacheci" "canteiro")
                   :deps ("build"))))
    "#;

    /// (a) The derive COMPILES on the `Vec<CiNodeSpec>` shape and a `(defci)`
    /// form parses via the generated `TataraDomain`; the parsed value then
    /// round-trips through serde byte-identically. Both together prove the
    /// authoring surface is real: `defci` is the keyword, the nested node list
    /// deserializes, and the type is a faithful serde value.
    #[test]
    fn derive_compiles_and_defci_form_round_trips_through_serde() {
        assert_eq!(CiSpec::KEYWORD, "defci");

        let mut specs = compile_typed::<CiSpec>(DEFCI_BUILD_TEST).expect("parse (defci …)");
        let spec = specs.pop().expect("one defci form");

        // The nested Vec<CiNodeSpec> parsed correctly.
        assert_eq!(spec.workspace, "pleme-io");
        assert_eq!(spec.repo, "sui");
        assert_eq!(spec.nodes.len(), 2);
        assert_eq!(spec.nodes[0].name, "build");
        assert_eq!(spec.nodes[0].args, vec!["build", "-p", "sui-supercacheci"]);
        assert_eq!(spec.nodes[0].inputs, vec!["sui-supercacheci/src/"]);
        // Omitted `:env` / `:deps` took their serde defaults.
        assert_eq!(spec.nodes[0].env, "none");
        assert!(spec.nodes[0].deps.is_empty());
        assert_eq!(spec.nodes[1].name, "test");
        assert_eq!(spec.nodes[1].deps, vec!["build"]);

        // serde round-trip: JSON value → back to CiSpec is byte-identical.
        let json = serde_json::to_value(&spec).expect("serialize");
        let back: CiSpec = serde_json::from_value(json).expect("deserialize");
        assert_eq!(spec, back, "CiSpec must round-trip through serde");
    }

    /// (b) The interpreter maps a 2-node `build → test` spec to a [`CiRun`]
    /// whose `decompose` yields the right DAG: `build` before `test`,
    /// `test.deps == [build]`, and the declared inputs are preserved.
    #[test]
    fn to_ci_run_maps_spec_to_a_run_whose_dag_orders_build_before_test() {
        let spec = compile_typed::<CiSpec>(DEFCI_BUILD_TEST)
            .expect("parse")
            .pop()
            .expect("one form");
        let run = spec.to_ci_run().expect("interpret");

        // The interpreted run carries the authored scope + node shape.
        assert_eq!(run.workspace, "pleme-io");
        assert_eq!(run.repo, "sui");
        assert_eq!(run.nodes.len(), 2);

        // test.deps == [build] (the authored edge survived the interpret).
        let test = run.nodes.iter().find(|n| n.name == "test").expect("test node");
        assert_eq!(test.deps, vec!["build".to_string()]);
        // Inputs are preserved on build (with_inputs was applied).
        let build = run.nodes.iter().find(|n| n.name == "build").expect("build node");
        assert_eq!(build.inputs, vec!["sui-supercacheci/src/".to_string()]);
        // env parsed to the typed class.
        assert_eq!(build.env_class, EnvClass::None);

        // decompose orders build before test.
        let cd = decompose(&run).expect("decompose");
        let order = cd.topo_order().expect("acyclic");
        let bi = order.iter().position(|j| *j == run.job_id("build")).unwrap();
        let ti = order.iter().position(|j| *j == run.job_id("test")).unwrap();
        assert!(bi < ti, "build must be ordered before test");
    }

    /// (c) THE FULL LOOP: a `(defci)`-authored form → `compile_typed` →
    /// `to_ci_run` → `emit_gha` yields a 2-job camelot GHA workflow with
    /// `test.needs == [build]`. This is the authoring half of leg 2 proven end
    /// to end against the shipped multi-worker emitter.
    #[test]
    fn full_loop_defci_to_ci_run_to_emit_gha_yields_two_job_graph() {
        let spec = compile_typed::<CiSpec>(DEFCI_BUILD_TEST)
            .expect("parse")
            .pop()
            .expect("one form");
        let run = spec.to_ci_run().expect("interpret");
        let wf = emit_gha(&run).expect("emit gha");

        assert_eq!(wf.jobs.len(), 2, "one GHA job per authored node");
        let build = wf.jobs.get("build").expect("build job");
        let test = wf.jobs.get("test").expect("test job");
        // The authored edge build→test is projected onto test.needs; build is a root.
        assert!(build.needs.is_empty(), "build is a root — no needs");
        assert_eq!(test.needs, vec!["build".to_string()], "test needs build");

        // Both jobs target the camelot ARC pool — asserted through the rendered
        // YAML (the `runs-on` field is private on the typed job).
        let yaml = emit_gha_yaml(&run).expect("render yaml");
        assert!(
            yaml.contains("camelot-builder-pleme-eks"),
            "jobs must target the camelot ARC pool"
        );
        // The real cargo actions carried through from the authored (defci) form.
        assert!(yaml.contains("cargo build -p sui-supercacheci"), "authored build action");
        assert!(
            yaml.contains("cargo test -p sui-supercacheci canteiro"),
            "authored test action"
        );
    }

    /// (d) A bad `:env` string is a typed [`CiSpecError`], never a panic and
    /// never a silent default.
    #[test]
    fn bad_env_string_is_a_typed_error_not_a_panic() {
        let src = r#"
            (defci :workspace "w" :repo "r"
              :nodes ((:name "x" :command "true" :env "wat")))
        "#;
        let spec = compile_typed::<CiSpec>(src).expect("parse").pop().expect("form");
        let err = spec.to_ci_run().expect_err("bad env must be a typed error");
        assert_eq!(
            err,
            CiSpecError::BadEnv {
                node: "x".to_string(),
                env: "wat".to_string(),
            }
        );
    }

    /// The `env` string parser covers every recognized class + both rejection
    /// arms — `localstack` and `warmpool:<ref>` map to their typed classes; a
    /// bare `warmpool:` is [`CiSpecError::EmptyWarmpoolRef`].
    #[test]
    fn parse_env_covers_every_class_and_both_rejections() {
        assert_eq!(parse_env("n", "none"), Ok(EnvClass::None));
        assert_eq!(parse_env("n", "localstack"), Ok(EnvClass::LocalStack));
        assert_eq!(
            parse_env("n", "warmpool:example-staging"),
            Ok(EnvClass::WarmPoolClaim("example-staging".to_string()))
        );
        assert_eq!(
            parse_env("n", "warmpool:"),
            Err(CiSpecError::EmptyWarmpoolRef { node: "n".to_string() })
        );
        assert_eq!(
            parse_env("n", "prod"),
            Err(CiSpecError::BadEnv {
                node: "n".to_string(),
                env: "prod".to_string(),
            })
        );
    }

    /// A `warmpool:<ref>` node interprets end to end into a [`CiRun`] carrying
    /// the typed [`EnvClass::WarmPoolClaim`] — the demand axis exists from the
    /// authoring surface even though the pool itself is DESIGN.
    #[test]
    fn to_ci_run_carries_a_warmpool_env_class() {
        let src = r#"
            (defci :workspace "w" :repo "r"
              :nodes ((:name "e2e" :command "true" :env "warmpool:live-tenant")))
        "#;
        let run = compile_typed::<CiSpec>(src)
            .expect("parse")
            .pop()
            .expect("form")
            .to_ci_run()
            .expect("interpret");
        assert_eq!(
            run.nodes[0].env_class,
            EnvClass::WarmPoolClaim("live-tenant".to_string())
        );
    }
}
