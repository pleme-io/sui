//! `sui eval --raw` emits the value's bytes and nothing else — like nix.
//!
//! # The defect
//!
//! sui appended a trailing newline that `nix eval --raw` does not, so every
//! `--raw` invocation differed from the oracle by one byte:
//!
//! ```text
//! sui eval --raw '…drvPath' > f    50 bytes, last byte 0a
//! nix eval --raw '…drvPath' > f    49 bytes, last byte 76
//! ```
//!
//! `--raw` exists so a caller can capture bytes VERBATIM, which is what makes
//! this a real parity gap rather than cosmetics. Command substitution hides it
//! — `$(…)` strips trailing newlines — but a redirect, a checksum of the
//! output, or a byte-compare does not. Anything that hashes sui's `--raw`
//! output and compares against nix's gets a different answer for a value the
//! two engines agree on.
//!
//! # How it was found, which is the instructive part
//!
//! An adversarial reviewer was checking a claim that sui and nix produced
//! "byte-for-byte" identical drvPaths for a NixOS toplevel. The VALUES were
//! identical — that part held. The CLI OUTPUT was not, and the reviewer's own
//! capture method is what exposed it: redirecting both to files and comparing
//! showed 50 vs 49 bytes. A claim of byte-identity was made about a comparison
//! that had never been done on bytes.
//!
//! # Why the tests assert on FILES, not on captured stdout
//!
//! `assert_cmd` hands back raw bytes, but the natural way to write this test —
//! comparing trimmed strings — would pass against the broken behaviour, since
//! the values differ only by the byte trimming removes. The whole defect lives
//! in the byte the convenient assertion discards.

use assert_cmd::Command;

const DRV: &str = "(derivation { name = \"t\"; system = \"aarch64-darwin\"; \
                   builder = \"/bin/sh\"; }).drvPath";

fn nix_raw(expr: &str) -> Option<Vec<u8>> {
    let out = std::process::Command::new("nix")
        .args(["eval", "--impure", "--raw", "--expr", expr])
        .output()
        .ok()?;
    out.status.success().then(|| out.stdout)
}

/// The bytes must match nix exactly, on BOTH engines.
///
/// Skipped when nix is unavailable — but note the skip is only honest because
/// the oracle-free rows below still run.
#[test]
fn raw_output_is_byte_identical_to_nix() {
    let Some(expected) = nix_raw(DRV) else {
        eprintln!("raw_output_is_byte_identical_to_nix: skipped (no usable nix)");
        return;
    };
    assert!(
        !expected.is_empty(),
        "the oracle produced no bytes — that is not a usable comparison"
    );

    for engine in [&[][..], &["--vm"][..]] {
        let mut cmd = Command::cargo_bin("sui").expect("cargo_bin sui");
        cmd.args(engine).args(["eval", "--raw", "-E", DRV]);
        let out = cmd.assert().success();
        let got = &out.get_output().stdout;
        assert_eq!(
            got, &expected,
            "engine {engine:?}: --raw bytes differ from nix\n  sui: {} bytes, last {:02x?}\n  nix: {} bytes, last {:02x?}",
            got.len(),
            got.last(),
            expected.len(),
            expected.last()
        );
    }
}

/// Oracle-free: `--raw` must not end in a newline.
///
/// This is the half that runs with no nix present, so the guard is not
/// entirely dependent on the oracle being installed.
#[test]
fn raw_output_has_no_trailing_newline() {
    let out = Command::cargo_bin("sui")
        .expect("cargo_bin sui")
        .args(["eval", "--raw", "-E", r#""hello""#])
        .assert()
        .success();
    let bytes = &out.get_output().stdout;
    assert_eq!(bytes, b"hello", "expected exactly `hello`, got {bytes:02x?}");
}

/// ★ CALIBRATION — non-raw modes must STILL end in a newline.
///
/// `nix eval` and `nix eval --json` both emit one. Without this row, "strip
/// the newline everywhere" would satisfy the assertions above perfectly while
/// creating the identical divergence in the opposite direction — which is the
/// failure shape this whole file is about.
#[test]
fn non_raw_output_still_ends_in_a_newline() {
    for args in [
        &["eval", "--json", "-E", "1"][..],
        &["eval", "-E", "1"][..],
    ] {
        let out = Command::cargo_bin("sui")
            .expect("cargo_bin sui")
            .args(args)
            .assert()
            .success();
        let bytes = &out.get_output().stdout;
        assert_eq!(
            bytes.last(),
            Some(&b'\n'),
            "{args:?}: non-raw output must end in a newline, got {bytes:02x?}"
        );
    }
}
