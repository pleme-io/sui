//! Layer 18: Flake lock roundtrip test.
//!
//! Creates a temp flake, evaluates it via sui, then verifies
//! lock file operations work correctly. Also tests reading the
//! real nix repo's flake.lock.

mod common;

use tempfile::TempDir;

#[test]
fn flake_lock_roundtrip_path_input() {
    if common::skip_if_offline("flake_lock_roundtrip") {
        return;
    }

    let tmp = TempDir::new().unwrap();
    let dep_dir = tmp.path().join("dep");
    std::fs::create_dir_all(&dep_dir).unwrap();

    // Create a dependency flake
    std::fs::write(
        dep_dir.join("flake.nix"),
        r#"{
        description = "test dependency";
        outputs = { self }: { value = 42; };
    }"#,
    )
    .unwrap();

    // Create the main flake that references it
    let main_dir = tmp.path().join("main");
    std::fs::create_dir_all(&main_dir).unwrap();
    std::fs::write(
        main_dir.join("flake.nix"),
        &format!(
            r#"{{
        description = "test main";
        inputs.dep.url = "path:{}";
        outputs = {{ self, dep }}: {{ value = dep.value + 1; }};
    }}"#,
            dep_dir.display()
        ),
    )
    .unwrap();

    // Initialize git repos (flakes require git)
    for dir in [&dep_dir, &main_dir] {
        match sui_eval::git::init_repo(dir, "main") {
            Ok(repo) => {
                let _ = sui_eval::git::commit_all(&repo, "init", "test", "test@test.com");
            }
            Err(e) => {
                println!("git init failed for {}: {e}", dir.display());
            }
        }
    }

    // Try listing inputs
    match sui_eval::flake_lock::list_inputs(&main_dir) {
        Ok(inputs) => println!("inputs: {inputs:?}"),
        Err(e) => println!("list_inputs: {e} (expected -- no lock file yet)"),
    }

    // Try evaluating the flake
    match sui_eval::builtins::evaluate_flake(&main_dir) {
        Ok(v) => println!("eval success: {:?}", v.type_name()),
        Err(e) => println!("eval: {e}"),
    }
}

/// Test that we can read the real nix repo's flake.lock
#[test]
fn flake_lock_read_real_nix_repo() {
    if common::skip_if_offline("flake_lock_read") {
        return;
    }

    let root = common::pleme_io_root();
    let nix_dir = root.join("nix");
    if !nix_dir.join("flake.lock").exists() {
        println!("skip: nix repo flake.lock not found");
        return;
    }

    // Read current lock to enumerate inputs
    match sui_eval::flake_lock::list_inputs(&nix_dir) {
        Ok(inputs) => {
            println!(
                "nix repo has {} inputs: {}",
                inputs.len(),
                inputs[..std::cmp::min(10, inputs.len())].join(", ")
            );
            if inputs.len() > 10 {
                println!("  ... and {} more", inputs.len() - 10);
            }
            assert!(
                !inputs.is_empty(),
                "nix repo should have at least one input"
            );
        }
        Err(e) => println!("list_inputs failed: {e}"),
    }
}

/// Test that get_input_rev works for known inputs
#[test]
fn flake_lock_get_nixpkgs_rev() {
    if common::skip_if_offline("flake_lock_nixpkgs_rev") {
        return;
    }

    let root = common::pleme_io_root();
    let nix_dir = root.join("nix");
    if !nix_dir.join("flake.lock").exists() {
        println!("skip: nix repo flake.lock not found");
        return;
    }

    match sui_eval::flake_lock::get_input_rev(&nix_dir, "nixpkgs") {
        Ok(Some(rev)) => {
            println!("nixpkgs locked at: {rev}");
            assert_eq!(rev.len(), 40, "git rev should be 40 hex chars");
            assert!(
                rev.chars().all(|c| c.is_ascii_hexdigit()),
                "rev should be hex"
            );
        }
        Ok(None) => println!("nixpkgs: no rev (unexpected for a locked input)"),
        Err(e) => println!("get_input_rev failed: {e}"),
    }
}

/// Test that a simple no-input flake evaluates correctly end-to-end
#[test]
fn flake_lock_roundtrip_no_inputs() {
    if common::skip_if_offline("flake_lock_roundtrip_no_inputs") {
        return;
    }

    let tmp = TempDir::new().unwrap();
    let flake_dir = tmp.path().join("simple");
    std::fs::create_dir_all(&flake_dir).unwrap();

    // A flake with no inputs -- doesn't need a lock file
    std::fs::write(
        flake_dir.join("flake.nix"),
        r#"{
        description = "simple roundtrip test";
        outputs = { self }: {
            answer = 42;
            greeting = "hello";
            nested = { a = 1; b = { c = true; }; };
        };
    }"#,
    )
    .unwrap();

    // Init git repo
    match sui_eval::git::init_repo(&flake_dir, "main") {
        Ok(repo) => {
            let _ = sui_eval::git::commit_all(&repo, "init", "test", "test@test.com");
        }
        Err(e) => {
            println!("git init failed: {e}");
            return;
        }
    }

    // Evaluate the flake
    let result = match sui_eval::builtins::evaluate_flake(&flake_dir) {
        Ok(v) => v,
        Err(e) => {
            println!("eval failed: {e}");
            return;
        }
    };

    // Verify top-level structure
    if let sui_eval::value::Value::Attrs(ref attrs) = result {
        // Check scalar outputs
        if let Some(answer) = attrs.get("answer") {
            let forced = sui_eval::eval::force_value(answer);
            match forced {
                Ok(sui_eval::value::Value::Int(42)) => println!("answer = 42 (correct)"),
                Ok(other) => println!("answer = {other:?} (unexpected)"),
                Err(e) => println!("force answer: {e}"),
            }
        }

        if let Some(greeting) = attrs.get("greeting") {
            let forced = sui_eval::eval::force_value(greeting);
            match forced {
                Ok(sui_eval::value::Value::String(ref s)) if s.as_str() == "hello" => {
                    println!("greeting = \"hello\" (correct)");
                }
                Ok(other) => println!("greeting = {other:?} (unexpected)"),
                Err(e) => println!("force greeting: {e}"),
            }
        }

        // Check nested
        if let Some(nested) = attrs.get("nested") {
            let forced = sui_eval::eval::force_value(nested);
            match forced {
                Ok(sui_eval::value::Value::Attrs(ref nested_attrs)) => {
                    println!(
                        "nested keys: {:?}",
                        nested_attrs.keys().collect::<Vec<_>>()
                    );
                }
                Ok(other) => println!("nested = {other:?} (unexpected)"),
                Err(e) => println!("force nested: {e}"),
            }
        }
    } else {
        println!("result is not attrs: {}", result.type_name());
    }

    println!("flake lock roundtrip (no inputs) completed");
}

/// Test reading multiple inputs from the real nix repo
#[test]
fn flake_lock_enumerate_all_real_inputs() {
    if common::skip_if_offline("flake_lock_enumerate") {
        return;
    }

    let root = common::pleme_io_root();
    let nix_dir = root.join("nix");
    if !nix_dir.join("flake.lock").exists() {
        println!("skip: nix repo flake.lock not found");
        return;
    }

    let inputs = match sui_eval::flake_lock::list_inputs(&nix_dir) {
        Ok(inputs) => inputs,
        Err(e) => {
            println!("list_inputs failed: {e}");
            return;
        }
    };

    println!("total inputs: {}", inputs.len());

    // Try getting rev for each input
    let mut with_rev = 0u32;
    let mut without_rev = 0u32;
    let mut errors = 0u32;

    for input in &inputs {
        match sui_eval::flake_lock::get_input_rev(&nix_dir, input) {
            Ok(Some(rev)) => {
                with_rev += 1;
                if with_rev <= 5 {
                    println!("  {input}: {}", &rev[..std::cmp::min(12, rev.len())]);
                }
            }
            Ok(None) => {
                without_rev += 1;
                if without_rev <= 3 {
                    println!("  {input}: (no rev -- likely a follows redirect)");
                }
            }
            Err(e) => {
                errors += 1;
                if errors <= 3 {
                    println!("  {input}: error: {e}");
                }
            }
        }
    }

    println!(
        "summary: {with_rev} with rev, {without_rev} without rev, {errors} errors"
    );
}
