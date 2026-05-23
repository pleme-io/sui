//! Property tests for the registry primitive's write/read
//! round-trip behavior.
//!
//! `sui registry add` serializes a typed RegistryEntry to JSON;
//! `registry::parse_entries` reads it back.  The round-trip
//! must be lossless for every valid combination of `from`,
//! `to`, and `exact`.

use proptest::prelude::*;
use sui_spec::registry::{self, RegistryEntry};

fn write_temp_registry(entries: &[RegistryEntry]) -> std::path::PathBuf {
    let id = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("sui-spec-reg-{id}-{nanos}.json"));

    // Mirror the same shape `cmd::registry_add` writes — flatten
    // refs to typed JSON objects.
    let flakes: Vec<serde_json::Value> = entries.iter().map(|e| {
        let from = ref_to_json(&e.from);
        let to = ref_to_json(&e.to);
        let mut obj = serde_json::json!({"from": from, "to": to});
        if e.exact {
            obj["exact"] = serde_json::Value::Bool(true);
        }
        obj
    }).collect();
    let doc = serde_json::json!({"version": 2, "flakes": flakes});
    std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    path
}

fn ref_to_json(s: &str) -> serde_json::Value {
    if let Some(rest) = s.strip_prefix("github:") {
        let parts: Vec<&str> = rest.splitn(3, '/').collect();
        match parts.as_slice() {
            [owner, repo]        => serde_json::json!({"type":"github","owner":owner,"repo":repo}),
            [owner, repo, r#ref] => serde_json::json!({"type":"github","owner":owner,"repo":repo,"ref":r#ref}),
            _                    => serde_json::json!({"type":"indirect","id":s}),
        }
    } else if let Some(url) = s.strip_prefix("git:") {
        serde_json::json!({"type":"git","url":url})
    } else if let Some(path) = s.strip_prefix("path:") {
        serde_json::json!({"type":"path","path":path})
    } else if let Some(url) = s.strip_prefix("tarball:") {
        serde_json::json!({"type":"tarball","url":url})
    } else {
        serde_json::json!({"type":"indirect","id":s})
    }
}

// ── Generator strategies ─────────────────────────────────────────

fn from_ref() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9-]{0,15}".prop_map(String::from)
}

fn to_ref() -> impl Strategy<Value = String> {
    prop_oneof![
        ("[a-z]{2,8}", "[a-z]{2,8}").prop_map(|(o, r)| format!("github:{o}/{r}")),
        ("[a-z]{2,8}", "[a-z]{2,8}", "[a-z]{2,8}").prop_map(|(o, r, ref_)| format!("github:{o}/{r}/{ref_}")),
        "[a-z]{2,8}".prop_map(String::from),
    ]
}

proptest! {
    /// Single-entry round-trip: write the entry, parse it back,
    /// byte-equal RegistryEntry.
    #[test]
    fn single_entry_roundtrips(
        from in from_ref(),
        to in to_ref(),
        exact in any::<bool>(),
    ) {
        let entry = RegistryEntry { from: from.clone(), to: to.clone(), exact };
        let path = write_temp_registry(std::slice::from_ref(&entry));
        let parsed = registry::load_entries_from_disk(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        prop_assert_eq!(parsed.len(), 1);
        prop_assert_eq!(&parsed[0].from, &from);
        prop_assert_eq!(&parsed[0].to, &to);
        prop_assert_eq!(parsed[0].exact, exact);
    }

    /// Multi-entry round-trip preserves order + content.
    #[test]
    fn multi_entry_roundtrips(
        froms in prop::collection::vec(from_ref(), 1..6),
    ) {
        // Build deterministic entries from the input froms.
        // Use the from-string as the to-string suffix so we can
        // verify each entry independently.
        let entries: Vec<RegistryEntry> = froms.iter().enumerate()
            .map(|(i, f)| RegistryEntry {
                from: f.clone(),
                to: format!("github:owner/repo-{i}"),
                exact: i % 2 == 0,
            })
            .collect();
        // De-dup by `from` because the JSON form would clobber.
        let mut seen = std::collections::HashSet::new();
        let unique: Vec<RegistryEntry> = entries.into_iter()
            .filter(|e| seen.insert(e.from.clone()))
            .collect();

        let path = write_temp_registry(&unique);
        let parsed = registry::load_entries_from_disk(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        prop_assert_eq!(parsed.len(), unique.len());
        for (a, b) in unique.iter().zip(parsed.iter()) {
            prop_assert_eq!(&a.from, &b.from);
            prop_assert_eq!(&a.to, &b.to);
            prop_assert_eq!(a.exact, b.exact);
        }
    }

    /// `resolve()` honours precedence — earliest scope wins.
    #[test]
    fn resolve_honors_precedence(
        flake_local_to in to_ref(),
        global_to in to_ref(),
    ) {
        use sui_spec::registry::{Registries, RegistryScope};
        let registries: Registries = vec![
            (RegistryScope::Global, vec![RegistryEntry {
                from: "x".into(), to: global_to.clone(), exact: false,
            }]),
            (RegistryScope::FlakeLocal, vec![RegistryEntry {
                from: "x".into(), to: flake_local_to.clone(), exact: true,
            }]),
        ];
        let resolved = registry::resolve(&registries, "x").unwrap();
        // FlakeLocal precedence wins.
        prop_assert_eq!(resolved.to, flake_local_to);
    }

    /// Unknown refs always error with the typed phase.
    #[test]
    fn unknown_ref_errors_typed(
        name in "[a-z]{8,15}",
    ) {
        use sui_spec::registry::{Registries, RegistryScope};
        let registries: Registries = vec![
            (RegistryScope::Global, vec![RegistryEntry {
                from: "known".into(), to: "github:x/y".into(), exact: false,
            }]),
        ];
        // Skip if generator happens to produce "known".
        prop_assume!(name != "known");
        let err = registry::resolve(&registries, &name).unwrap_err();
        match err {
            sui_spec::SpecError::Interp { phase, .. } => {
                prop_assert_eq!(phase, "registry-unresolved");
            }
            _ => prop_assert!(false, "expected registry-unresolved"),
        }
    }
}

#[test]
fn missing_registry_file_yields_empty_entries() {
    let path = std::path::Path::new("/nonexistent/x.json");
    let entries = registry::load_entries_from_disk(path).unwrap();
    assert!(entries.is_empty());
}
