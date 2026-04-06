//! Layer 3: flake.lock parity on real pleme-io lock files.
//!
//! Walks every `flake.lock` under `PLEME_IO_ROOT` (lex-sorted, first N)
//! and verifies:
//!
//! 1. `FlakeLock::parse` succeeds without panic or error.
//! 2. `parse → to_json → parse` yields a structurally-equal lock —
//!    i.e., the serializer preserves enough to reconstruct the
//!    original graph, even if key order shifts.
//! 3. `root_inputs()` resolves every top-level input to an existing
//!    node. Any `follows` path that dead-ends surfaces as a failure.
//! 4. Every `InputRef::Follows` in every node resolves successfully.
//!
//! Offline test — just reads files and calls the library.

use std::path::PathBuf;
use sui_compat::flake::{FlakeLock, InputRef};

/// How many flake.lock files to sample.
const LOCK_SAMPLE_SIZE: usize = 200;

fn pleme_io_root() -> PathBuf {
    if let Ok(v) = std::env::var("PLEME_IO_ROOT") {
        return PathBuf::from(v);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join("code/github/pleme-io")
}

fn sample_flake_locks(n: usize) -> Vec<PathBuf> {
    let root = pleme_io_root();
    if !root.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
    walk(&root, "flake.lock", 3, &mut out);
    out.sort();
    out.truncate(n);
    out
}

fn walk(dir: &std::path::Path, file_name: &str, depth_remaining: usize, out: &mut Vec<PathBuf>) {
    const SKIP: &[&str] = &[".git", "target", "node_modules", "result", "dist", "build", ".direnv"];
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if path.is_file() && name == file_name {
            out.push(path);
            continue;
        }
        if path.is_dir() && depth_remaining > 0 {
            if name.starts_with('.') && name != "." {
                continue;
            }
            if SKIP.contains(&name.as_str()) || name.starts_with("result-") {
                continue;
            }
            walk(&path, file_name, depth_remaining - 1, out);
        }
    }
}

#[test]
fn parse_all_pleme_io_flake_locks() {
    let corpus = sample_flake_locks(LOCK_SAMPLE_SIZE);
    if corpus.is_empty() {
        eprintln!(
            "skip parse_all_pleme_io_flake_locks: no flake.lock under {}",
            pleme_io_root().display()
        );
        return;
    }

    let mut parsed = 0usize;
    let mut failures: Vec<(PathBuf, String)> = Vec::new();

    for path in &corpus {
        let text = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                failures.push((path.clone(), format!("read: {e}")));
                continue;
            }
        };
        match FlakeLock::parse(&text) {
            Ok(_) => parsed += 1,
            Err(e) => failures.push((path.clone(), format!("{e}"))),
        }
    }

    eprintln!(
        "parse_all_pleme_io_flake_locks: parsed {parsed} / {}, failures {}",
        corpus.len(),
        failures.len()
    );
    if !failures.is_empty() {
        let summary: String = failures
            .iter()
            .take(10)
            .map(|(p, e)| format!("  {}\n    {e}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "{} / {} flake.lock files failed to parse. First {}:\n{}",
            failures.len(),
            corpus.len(),
            failures.len().min(10),
            summary
        );
    }
}

/// Return a human-readable hint about the first node that differs
/// between two flake.lock JSON values. Returns `None` if the top-level
/// shape is unexpected.
fn first_node_diff(
    orig: &serde_json::Value,
    rt: &serde_json::Value,
) -> Option<String> {
    let orig_nodes = orig.get("nodes")?.as_object()?;
    let rt_nodes = rt.get("nodes")?.as_object()?;
    for (name, node) in orig_nodes {
        let Some(rt_node) = rt_nodes.get(name) else {
            return Some(format!("node {name} missing in round-trip"));
        };
        if node != rt_node {
            return Some(format!(
                "first diff in node {name}\n    orig: {}\n    rt:   {}",
                serde_json::to_string(node).unwrap_or_default(),
                serde_json::to_string(rt_node).unwrap_or_default(),
            ));
        }
    }
    for name in rt_nodes.keys() {
        if !orig_nodes.contains_key(name) {
            return Some(format!("extra node {name} in round-trip"));
        }
    }
    // Top-level non-nodes diff
    Some(format!(
        "top-level diff (not in nodes)\n    orig: {}\n    rt:   {}",
        serde_json::to_string(orig).unwrap_or_default(),
        serde_json::to_string(rt).unwrap_or_default(),
    ))
}

/// Walk a JSON value and drop every `null` so we can compare the
/// original file against sui's round-tripped output without tripping
/// on `None` vs "absent". Real flake.lock files omit optional fields
/// rather than emitting them as `null`, so both normalized forms
/// should be structurally equal.
fn strip_nulls(v: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(_, vv)| !vv.is_null())
                .map(|(k, vv)| (k, strip_nulls(vv)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}

#[test]
fn parse_reserialize_reparse_is_stable() {
    // parse → to_json → parse: the normalized JSON representations
    // must be structurally equal. We drop null fields on both sides
    // because real lock files omit optional fields while sui's
    // serializer emits them as `null`. We compare as
    // `serde_json::Value` so key order and whitespace don't matter.
    //
    // This catches:
    //   - fields silently dropped by the serializer
    //   - fields the parser doesn't preserve
    //   - inconsistent handling of optional fields beyond null vs absent
    let corpus = sample_flake_locks(LOCK_SAMPLE_SIZE);
    if corpus.is_empty() {
        eprintln!("skip parse_reserialize_reparse_is_stable: no corpus");
        return;
    }

    let mut checked = 0usize;
    let mut drift: Vec<(PathBuf, String)> = Vec::new();

    for path in &corpus {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(first) = FlakeLock::parse(&text) else {
            continue;
        };
        let roundtripped_json = match first.to_json() {
            Ok(s) => s,
            Err(e) => {
                drift.push((path.clone(), format!("to_json: {e}")));
                continue;
            }
        };
        // Re-parse via FlakeLock to confirm the serializer output is
        // at least valid input to the parser.
        if let Err(e) = FlakeLock::parse(&roundtripped_json) {
            drift.push((path.clone(), format!("reparse: {e}")));
            continue;
        }
        // Structural compare after stripping null fields on both sides.
        let Ok(orig_val) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Ok(rt_val) = serde_json::from_str::<serde_json::Value>(&roundtripped_json) else {
            continue;
        };
        let orig_norm = strip_nulls(orig_val);
        let rt_norm = strip_nulls(rt_val);
        if orig_norm != rt_norm {
            // Locate the first node that differs, for useful output.
            let hint = first_node_diff(&orig_norm, &rt_norm).unwrap_or_default();
            drift.push((
                path.clone(),
                format!("parse→to_json produced a structurally different JSON\n    {hint}"),
            ));
        }
        checked += 1;
    }

    eprintln!(
        "parse_reserialize_reparse_is_stable: checked {checked}, drift {}",
        drift.len()
    );
    if !drift.is_empty() {
        let summary: String = drift
            .iter()
            .take(5)
            .map(|(p, e)| format!("  {}\n    {e}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "{} flake.lock files drifted on round-trip. First {}:\n{}",
            drift.len(),
            drift.len().min(5),
            summary
        );
    }
}

#[test]
fn root_inputs_resolve_to_existing_nodes() {
    let corpus = sample_flake_locks(LOCK_SAMPLE_SIZE);
    if corpus.is_empty() {
        eprintln!("skip root_inputs_resolve_to_existing_nodes: no corpus");
        return;
    }

    let mut checked = 0usize;
    let mut failures: Vec<(PathBuf, String)> = Vec::new();

    for path in &corpus {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(lock) = FlakeLock::parse(&text) else {
            continue;
        };
        match lock.root_inputs() {
            Ok(inputs) => {
                for (name, node_name) in inputs {
                    if lock.get_node(&node_name).is_err() {
                        failures.push((
                            path.clone(),
                            format!("root input {name} → {node_name} (missing)"),
                        ));
                    }
                }
            }
            Err(e) => failures.push((path.clone(), format!("root_inputs: {e}"))),
        }
        checked += 1;
    }

    eprintln!(
        "root_inputs_resolve: checked {checked}, failures {}",
        failures.len()
    );
    if !failures.is_empty() {
        let summary: String = failures
            .iter()
            .take(5)
            .map(|(p, e)| format!("  {}\n    {e}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "{} lock files had root-input resolution failures. First {}:\n{}",
            failures.len(),
            failures.len().min(5),
            summary
        );
    }
}

#[test]
fn every_follows_resolves() {
    // Walk every node's inputs; for every Follows reference, confirm
    // resolve_ref succeeds and points at a node that exists.
    let corpus = sample_flake_locks(LOCK_SAMPLE_SIZE);
    if corpus.is_empty() {
        eprintln!("skip every_follows_resolves: no corpus");
        return;
    }

    let mut total_follows = 0usize;
    let mut failures: Vec<(PathBuf, String)> = Vec::new();

    for path in &corpus {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(lock) = FlakeLock::parse(&text) else {
            continue;
        };
        for (node_name, node) in &lock.nodes {
            for (input_name, input_ref) in &node.inputs {
                if matches!(input_ref, InputRef::Follows(_)) {
                    total_follows += 1;
                    match lock.resolve_ref(node_name, input_ref) {
                        Ok(resolved) => {
                            if lock.get_node(&resolved).is_err() {
                                failures.push((
                                    path.clone(),
                                    format!("{node_name}.{input_name} → {resolved} (missing)"),
                                ));
                            }
                        }
                        Err(e) => failures.push((
                            path.clone(),
                            format!("{node_name}.{input_name}: {e}"),
                        )),
                    }
                }
            }
        }
    }

    eprintln!(
        "every_follows_resolves: {total_follows} follows refs, {} failures",
        failures.len()
    );
    if !failures.is_empty() {
        let summary: String = failures
            .iter()
            .take(10)
            .map(|(p, e)| format!("  {}\n    {e}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "{} follows refs failed to resolve. First {}:\n{}",
            failures.len(),
            failures.len().min(10),
            summary
        );
    }
}
