//! Convergence computing builtins.
//!
//! Extends sui with `builtins.sui.convergence.*` for declaring typed
//! convergence points, DAGs, and compliance packages as Nix expressions.

use std::rc::Rc;

use super::*;

/// Register convergence builtins into the `sui.convergence` namespace.
pub(crate) fn register(attrs: &mut NixAttrs) {
    let mut convergence = NixAttrs::new();

    // builtins.sui.convergence.point { name, type, substrate, ... }
    register_builtin(&mut convergence, "point", |args| {
        let input = args[0].to_attrs().map_err(|_| {
            EvalError::TypeError("convergence.point: expected attribute set".into())
        })?;

        let name = input
            .get("name")
            .ok_or_else(|| EvalError::TypeError("convergence.point: missing 'name'".into()))?
            .as_string()?
            .to_string();

        let point_type = input
            .get("type")
            .and_then(|v| v.as_string().ok())
            .unwrap_or("transform")
            .to_string();

        let substrate = input
            .get("substrate")
            .and_then(|v| v.as_string().ok())
            .unwrap_or("compute")
            .to_string();

        let horizon = input
            .get("horizon")
            .and_then(|v| v.as_string().ok())
            .unwrap_or("bounded")
            .to_string();

        let mode = input
            .get("mode")
            .and_then(|v| v.as_string().ok())
            .unwrap_or("mechanical")
            .to_string();

        let mut result = NixAttrs::new();
        result.insert("_type".into(), Value::string("convergence-point"));
        result.insert("name".into(), Value::string(name));
        result.insert("pointType".into(), Value::string(point_type));
        result.insert("substrate".into(), Value::string(substrate));
        result.insert("horizon".into(), Value::string(horizon));
        result.insert("computationMode".into(), Value::string(mode));

        // Pass through additional attributes
        for key in ["preconditions", "postconditions", "description", "convergence"] {
            if let Some(val) = input.get(key) {
                result.insert(key.into(), val.clone());
            }
        }

        Ok(Value::Attrs(Rc::new(result)))
    });

    // builtins.sui.convergence.dag { points, edges }
    register_builtin(&mut convergence, "dag", |args| {
        let input = args[0].to_attrs().map_err(|_| {
            EvalError::TypeError("convergence.dag: expected attribute set".into())
        })?;

        let points = input
            .get("points")
            .ok_or_else(|| EvalError::TypeError("convergence.dag: missing 'points'".into()))?
            .clone();

        let edges = input
            .get("edges")
            .cloned()
            .unwrap_or_else(|| Value::list(vec![]));

        let substrate = input
            .get("substrate")
            .and_then(|v| v.as_string().ok())
            .unwrap_or("compute")
            .to_string();

        let mut result = NixAttrs::new();
        result.insert("_type".into(), Value::string("convergence-dag"));
        result.insert("substrate".into(), Value::string(substrate));
        result.insert("points".into(), points);
        result.insert("edges".into(), edges);

        Ok(Value::Attrs(Rc::new(result)))
    });

    // builtins.sui.convergence.graph { substrates, crossEdges }
    register_builtin(&mut convergence, "graph", |args| {
        let input = args[0].to_attrs().map_err(|_| {
            EvalError::TypeError("convergence.graph: expected attribute set".into())
        })?;

        let substrates = input
            .get("substrates")
            .ok_or_else(|| {
                EvalError::TypeError("convergence.graph: missing 'substrates'".into())
            })?
            .clone();

        let cross_edges = input
            .get("crossEdges")
            .cloned()
            .unwrap_or_else(|| Value::list(vec![]));

        let compliance = input
            .get("compliance")
            .cloned()
            .unwrap_or_else(|| Value::list(vec![]));

        let mut result = NixAttrs::new();
        result.insert("_type".into(), Value::string("convergence-graph"));
        result.insert("substrates".into(), substrates);
        result.insert("crossEdges".into(), cross_edges);
        result.insert("compliance".into(), compliance);

        Ok(Value::Attrs(Rc::new(result)))
    });

    // builtins.sui.convergence.compliancePackage { framework, controls, bindings }
    register_builtin(&mut convergence, "compliancePackage", |args| {
        let input = args[0].to_attrs().map_err(|_| {
            EvalError::TypeError(
                "convergence.compliancePackage: expected attribute set".into(),
            )
        })?;

        let framework = input
            .get("framework")
            .ok_or_else(|| {
                EvalError::TypeError(
                    "convergence.compliancePackage: missing 'framework'".into(),
                )
            })?
            .as_string()?
            .to_string();

        let mut result = NixAttrs::new();
        result.insert("_type".into(), Value::string("compliance-package"));
        result.insert("framework".into(), Value::string(framework));

        for key in ["controls", "bindings", "baseline"] {
            if let Some(val) = input.get(key) {
                result.insert(key.into(), val.clone());
            }
        }

        Ok(Value::Attrs(Rc::new(result)))
    });

    attrs.insert("convergence".into(), Value::Attrs(Rc::new(convergence)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_convergence_builtin(name: &str, input: NixAttrs) -> Result<Value, EvalError> {
        let mut root = NixAttrs::new();
        register(&mut root);

        let conv_attrs = match root.get("convergence") {
            Some(Value::Attrs(a)) => a.clone(),
            _ => panic!("convergence not registered"),
        };

        let builtin = match conv_attrs.get(name) {
            Some(Value::Builtin(bf)) => bf.clone(),
            _ => panic!("builtin {name} not found"),
        };

        (builtin.func)(&[Value::Attrs(Rc::new(input))])
    }

    #[test]
    fn test_convergence_point() {
        let mut input = NixAttrs::new();
        input.insert("name".into(), Value::string("secret_resolve"));
        input.insert("type".into(), Value::string("transform"));
        input.insert("substrate".into(), Value::string("security"));

        let result = eval_convergence_builtin("point", input).unwrap();
        let attrs = result.to_attrs().unwrap();

        assert_eq!(attrs.get("_type").unwrap().as_string().unwrap(), "convergence-point");
        assert_eq!(attrs.get("name").unwrap().as_string().unwrap(), "secret_resolve");
        assert_eq!(attrs.get("pointType").unwrap().as_string().unwrap(), "transform");
        assert_eq!(attrs.get("substrate").unwrap().as_string().unwrap(), "security");
    }

    #[test]
    fn test_convergence_point_defaults() {
        let mut input = NixAttrs::new();
        input.insert("name".into(), Value::string("test"));

        let result = eval_convergence_builtin("point", input).unwrap();
        let attrs = result.to_attrs().unwrap();

        assert_eq!(attrs.get("pointType").unwrap().as_string().unwrap(), "transform");
        assert_eq!(attrs.get("substrate").unwrap().as_string().unwrap(), "compute");
        assert_eq!(attrs.get("horizon").unwrap().as_string().unwrap(), "bounded");
        assert_eq!(attrs.get("computationMode").unwrap().as_string().unwrap(), "mechanical");
    }

    #[test]
    fn test_convergence_point_missing_name() {
        let input = NixAttrs::new();
        let result = eval_convergence_builtin("point", input);
        assert!(result.is_err());
    }

    #[test]
    fn test_convergence_dag() {
        let mut points = NixAttrs::new();
        points.insert("a".into(), Value::Null);

        let mut input = NixAttrs::new();
        input.insert("points".into(), Value::Attrs(Rc::new(points)));
        input.insert("substrate".into(), Value::string("compute"));

        let result = eval_convergence_builtin("dag", input).unwrap();
        let attrs = result.to_attrs().unwrap();

        assert_eq!(attrs.get("_type").unwrap().as_string().unwrap(), "convergence-dag");
    }

    #[test]
    fn test_convergence_graph() {
        let mut substrates = NixAttrs::new();
        substrates.insert("compute".into(), Value::Null);

        let mut input = NixAttrs::new();
        input.insert("substrates".into(), Value::Attrs(Rc::new(substrates)));

        let result = eval_convergence_builtin("graph", input).unwrap();
        let attrs = result.to_attrs().unwrap();

        assert_eq!(attrs.get("_type").unwrap().as_string().unwrap(), "convergence-graph");
    }

    #[test]
    fn test_compliance_package() {
        let mut input = NixAttrs::new();
        input.insert("framework".into(), Value::string("nist-800-53"));
        input.insert("baseline".into(), Value::string("moderate"));

        let result = eval_convergence_builtin("compliancePackage", input).unwrap();
        let attrs = result.to_attrs().unwrap();

        assert_eq!(attrs.get("_type").unwrap().as_string().unwrap(), "compliance-package");
        assert_eq!(attrs.get("framework").unwrap().as_string().unwrap(), "nist-800-53");
    }

    #[test]
    fn test_compliance_package_missing_framework() {
        let input = NixAttrs::new();
        let result = eval_convergence_builtin("compliancePackage", input);
        assert!(result.is_err());
    }
}
