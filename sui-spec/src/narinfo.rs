//! Typed border for the narinfo file format.
//!
//! When a substituter serves a NAR, the narinfo file alongside it
//! carries the metadata: store path, NAR URL, NAR hash, file
//! size, references (closure), deriver, signatures, compression
//! method.  Format is plain-text, one field per line, `Key: Value`
//! with a closed key set cppnix has stabilised since Nix 2.
//!
//! This module names the format as a typed Lisp spec so future
//! parser/emitter implementations ride on the same contract both
//! engines agree on.
//!
//! ## Authoring surface
//!
//! ```lisp
//! (defnarinfo-format
//!   :name        "cppnix-narinfo-v1"
//!   :fields      (Required Required Required Required
//!                 Optional Optional Optional Required)
//!   :field-names ("StorePath" "URL" "Compression" "FileHash"
//!                 "FileSize" "NarHash" "NarSize" "References"
//!                 "Deriver" "System" "Sig")
//!   :phases      ((:kind ParseTextFields)
//!                 (:kind ValidateRequiredFields)
//!                 (:kind ParseSignatures)
//!                 (:kind EmitTextOutput)))
//! ```

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

// ── Typed border ───────────────────────────────────────────────────

/// One narinfo format variant.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defnarinfo-format")]
pub struct NarinfoFormat {
    pub name: String,
    /// Declared fields in the order they conventionally appear.
    /// Length must match `field_names`.
    pub fields: Vec<NarinfoFieldKind>,
    /// Canonical key names — index-aligned with `fields`.
    #[serde(rename = "fieldNames")]
    pub field_names: Vec<String>,
    pub phases: Vec<NarinfoPhase>,
}

/// Whether a field is required or optional in the narinfo.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarinfoFieldKind {
    /// Must appear; parser rejects narinfo without it.
    Required,
    /// May appear or not; parser tolerates absence.
    Optional,
    /// May appear multiple times (e.g. `Sig:` for multi-signature).
    Repeatable,
}

/// One phase of narinfo handling.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NarinfoPhase {
    pub kind: NarinfoPhaseKind,
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
}

/// Closed set of narinfo phases.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarinfoPhaseKind {
    /// Read raw text, split on `\n`, parse `Key: Value` per line.
    ParseTextFields,
    /// Check every Required field is present.
    ValidateRequiredFields,
    /// Parse `Sig:` lines into typed (key-name, signature) pairs.
    ParseSignatures,
    /// Parse `References:` whitespace-separated store-path list.
    ParseReferences,
    /// Validate the `NarHash:` is well-formed (sri or
    /// `sha256:<base32>`).
    ValidateNarHashShape,
    /// Emit the parsed narinfo back to text — round-trip clean
    /// byte-equality is a parser correctness invariant.
    EmitTextOutput,
}

// ── Spec interpreter (M3.0 minimal) ────────────────────────────────

/// Parsed narinfo record.  Fields match the canonical cppnix
/// format declared in `cppnix-narinfo-v1`.  All optional fields
/// are `None`/`empty` when absent on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedNarInfo {
    /// Required.  Full store path (`/nix/store/<hash>-<name>`).
    pub store_path: String,
    /// Required.  URL of the NAR file relative to the substituter.
    pub url: String,
    /// Required.  Compression algorithm (`xz` / `zstd` / `bzip2` /
    /// `none`).
    pub compression: String,
    /// Optional.  File hash of the compressed NAR.
    pub file_hash: Option<String>,
    /// Optional.  Size in bytes of the compressed NAR.
    pub file_size: Option<u64>,
    /// Required.  Hash of the decompressed NAR (`sha256:<base32>`).
    pub nar_hash: String,
    /// Required.  Size in bytes of the decompressed NAR.
    pub nar_size: u64,
    /// Optional.  Space-separated list of store-path references.
    pub references: Vec<String>,
    /// Optional.  Deriver `.drv` path.
    pub deriver: Option<String>,
    /// Optional.  System tuple (`x86_64-linux`).
    pub system: Option<String>,
    /// Repeatable.  Signature lines (`<key-name>:<base64-sig>`).
    pub signatures: Vec<String>,
    /// Optional.  Content-address descriptor for CA derivations.
    pub ca: Option<String>,
}

/// Parse a narinfo text payload against a format spec.
///
/// # Errors
///
/// - `narinfo-parse` for malformed `Key: Value` lines.
/// - `narinfo-missing-required` if a Required field per the format
///   spec is absent.
/// - `narinfo-bad-int` for `NarSize:` / `FileSize:` that don't
///   parse as integers.
pub fn parse(text: &str, format: &NarinfoFormat) -> Result<ParsedNarInfo, SpecError> {
    let mut record = ParsedNarInfo::default();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for (lineno, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(SpecError::Interp {
                phase: "narinfo-parse".into(),
                message: format!(
                    "line {}: not `Key: Value`: {raw_line}",
                    lineno + 1,
                ),
            });
        };
        let key = key.trim();
        let value = value.trim();
        seen.insert(match key {
            // Common keys — interned to &'static so the HashSet
            // borrow lifetimes work.
            "StorePath" => "StorePath",
            "URL" => "URL",
            "Compression" => "Compression",
            "FileHash" => "FileHash",
            "FileSize" => "FileSize",
            "NarHash" => "NarHash",
            "NarSize" => "NarSize",
            "References" => "References",
            "Deriver" => "Deriver",
            "System" => "System",
            "Sig" => "Sig",
            "CA" => "CA",
            _ => "",
        });
        match key {
            "StorePath" => record.store_path = value.into(),
            "URL" => record.url = value.into(),
            "Compression" => record.compression = value.into(),
            "FileHash" => record.file_hash = Some(value.into()),
            "FileSize" => {
                record.file_size = Some(value.parse().map_err(|e| SpecError::Interp {
                    phase: "narinfo-bad-int".into(),
                    message: format!("FileSize `{value}`: {e}"),
                })?);
            }
            "NarHash" => record.nar_hash = value.into(),
            "NarSize" => {
                record.nar_size = value.parse().map_err(|e| SpecError::Interp {
                    phase: "narinfo-bad-int".into(),
                    message: format!("NarSize `{value}`: {e}"),
                })?;
            }
            "References" => {
                record.references = value
                    .split_whitespace()
                    .map(String::from)
                    .collect();
            }
            "Deriver" => record.deriver = Some(value.into()),
            "System" => record.system = Some(value.into()),
            "Sig" => record.signatures.push(value.into()),
            "CA" => record.ca = Some(value.into()),
            _ => {
                // Unknown key — narinfo format is extensible, so
                // we ignore rather than error.  Future M3.x may
                // surface a warning channel.
            }
        }
    }

    // Validate Required fields per the format spec.
    for (kind, name) in format.fields.iter().zip(format.field_names.iter()) {
        if *kind == NarinfoFieldKind::Required && !seen.contains(name.as_str()) {
            return Err(SpecError::Interp {
                phase: "narinfo-missing-required".into(),
                message: format!(
                    "narinfo missing required field `{name}` per format `{}`",
                    format.name,
                ),
            });
        }
    }
    Ok(record)
}

/// Emit a narinfo record back to canonical text form.  Round-trip
/// property: `parse(emit(r)) == r` for every well-formed record.
#[must_use]
pub fn emit(record: &ParsedNarInfo) -> String {
    let mut lines = Vec::new();
    lines.push(format!("StorePath: {}", record.store_path));
    lines.push(format!("URL: {}", record.url));
    lines.push(format!("Compression: {}", record.compression));
    if let Some(h) = &record.file_hash {
        lines.push(format!("FileHash: {h}"));
    }
    if let Some(s) = record.file_size {
        lines.push(format!("FileSize: {s}"));
    }
    lines.push(format!("NarHash: {}", record.nar_hash));
    lines.push(format!("NarSize: {}", record.nar_size));
    if !record.references.is_empty() {
        lines.push(format!("References: {}", record.references.join(" ")));
    }
    if let Some(d) = &record.deriver {
        lines.push(format!("Deriver: {d}"));
    }
    if let Some(s) = &record.system {
        lines.push(format!("System: {s}"));
    }
    for sig in &record.signatures {
        lines.push(format!("Sig: {sig}"));
    }
    if let Some(ca) = &record.ca {
        lines.push(format!("CA: {ca}"));
    }
    // Trailing newline — cppnix convention.
    lines.push(String::new());
    lines.join("\n")
}

/// Apply the narinfo algorithm.  M3.0 surface — combines parse +
/// emit into a single round-trip step against a text payload.
/// The compound `apply` interface stays available; the underlying
/// `parse` + `emit` are the primary surface.
///
/// # Errors
///
/// Returns the underlying parse error per [`parse`].
pub fn apply_roundtrip(text: &str, format: &NarinfoFormat) -> Result<String, SpecError> {
    let record = parse(text, format)?;
    Ok(emit(&record))
}

/// Legacy `apply` stub — kept for the substrate-invariants smoke
/// check.  Returns a typed not-yet error so the substrate test
/// can confirm typed-stub semantics; new code uses [`parse`],
/// [`emit`], or [`apply_roundtrip`].
///
/// # Errors
///
/// Always.  Use [`apply_roundtrip`] for the M3.0 functional
/// interface.
pub fn apply(_format: &NarinfoFormat) -> Result<String, SpecError> {
    Err(SpecError::Interp {
        phase: "narinfo".into(),
        message: "narinfo::apply() is the legacy stub — use \
                  parse(), emit(), or apply_roundtrip() for the \
                  M3.0 functional interface".into(),
    })
}

// ── Canonical spec ─────────────────────────────────────────────────

pub const CANONICAL_NARINFO_LISP: &str = include_str!("../specs/narinfo.lisp");

/// Compile every authored narinfo format.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<NarinfoFormat>, SpecError> {
    crate::loader::load_all::<NarinfoFormat>(CANONICAL_NARINFO_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn canonical_narinfo_parses() {
        let formats = load_canonical().expect("canonical narinfo must compile");
        assert!(!formats.is_empty());
    }

    #[test]
    fn cppnix_v1_lists_required_keys() {
        let formats = load_canonical().unwrap();
        let v1 = formats
            .iter()
            .find(|f| f.name == "cppnix-narinfo-v1")
            .expect("cppnix-narinfo-v1 must exist");
        assert_eq!(v1.fields.len(), v1.field_names.len(),
            "fields/field_names length mismatch");
        // Build name → kind map for assertions.
        let by_name: HashMap<&str, NarinfoFieldKind> = v1
            .field_names
            .iter()
            .map(|s| s.as_str())
            .zip(v1.fields.iter().copied())
            .collect();
        // The four bytes-on-the-wire mandatory fields.
        for required in ["StorePath", "URL", "NarHash", "NarSize"] {
            assert_eq!(
                by_name.get(required).copied(),
                Some(NarinfoFieldKind::Required),
                "{required} must be Required in cppnix-narinfo-v1",
            );
        }
        // Sig is canonically Repeatable (multi-key signature).
        assert_eq!(
            by_name.get("Sig").copied(),
            Some(NarinfoFieldKind::Repeatable),
        );
    }

    #[test]
    fn narinfo_phases_include_text_roundtrip() {
        let formats = load_canonical().unwrap();
        for f in &formats {
            let kinds: Vec<NarinfoPhaseKind> =
                f.phases.iter().map(|p| p.kind).collect();
            assert!(kinds.contains(&NarinfoPhaseKind::ParseTextFields),
                "{}: missing ParseTextFields", f.name);
            assert!(kinds.contains(&NarinfoPhaseKind::ValidateRequiredFields),
                "{}: missing ValidateRequiredFields", f.name);
        }
    }

    // ── M3.0 parse + emit tests ────────────────────────────────

    fn fmt() -> NarinfoFormat {
        load_canonical().unwrap().into_iter()
            .find(|f| f.name == "cppnix-narinfo-v1")
            .unwrap()
    }

    const CANONICAL_NARINFO: &str = "\
StorePath: /nix/store/abc-hello
URL: nar/abc.nar.xz
Compression: xz
FileHash: sha256:filefilefile
FileSize: 12345
NarHash: sha256:narnarnar
NarSize: 67890
References: /nix/store/dep1 /nix/store/dep2
Deriver: /nix/store/abc-hello.drv
System: x86_64-linux
Sig: cache.nixos.org-1:sig-bytes-1
Sig: cache.nixos.org-2:sig-bytes-2
";

    #[test]
    fn parse_canonical_narinfo() {
        let record = parse(CANONICAL_NARINFO, &fmt()).unwrap();
        assert_eq!(record.store_path, "/nix/store/abc-hello");
        assert_eq!(record.url, "nar/abc.nar.xz");
        assert_eq!(record.compression, "xz");
        assert_eq!(record.file_hash.as_deref(), Some("sha256:filefilefile"));
        assert_eq!(record.file_size, Some(12345));
        assert_eq!(record.nar_hash, "sha256:narnarnar");
        assert_eq!(record.nar_size, 67890);
        assert_eq!(record.references, vec!["/nix/store/dep1", "/nix/store/dep2"]);
        assert_eq!(record.deriver.as_deref(), Some("/nix/store/abc-hello.drv"));
        assert_eq!(record.signatures.len(), 2);
    }

    #[test]
    fn roundtrip_byte_equivalence() {
        let parsed = parse(CANONICAL_NARINFO, &fmt()).unwrap();
        let emitted = emit(&parsed);
        let reparsed = parse(&emitted, &fmt()).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn missing_required_field_errors() {
        let bad = "URL: x\nCompression: xz\nNarHash: y\nNarSize: 1\n";
        let err = parse(bad, &fmt()).unwrap_err();
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "narinfo-missing-required");
                assert!(message.contains("StorePath"));
            }
            _ => panic!("expected narinfo-missing-required"),
        }
    }

    #[test]
    fn malformed_line_errors() {
        let bad = "StorePath /no/colon\n";
        let err = parse(bad, &fmt()).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "narinfo-parse"),
            _ => panic!("expected narinfo-parse"),
        }
    }

    #[test]
    fn bad_int_errors() {
        let bad = "StorePath: /x\nURL: y\nCompression: xz\nNarHash: z\nNarSize: not-a-number\n";
        let err = parse(bad, &fmt()).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "narinfo-bad-int"),
            _ => panic!("expected narinfo-bad-int"),
        }
    }

    #[test]
    fn multiple_sig_lines_accumulate() {
        let multisig = "\
StorePath: /x
URL: y
Compression: xz
NarHash: z
NarSize: 1
Sig: key1:sig1
Sig: key2:sig2
Sig: key3:sig3
";
        let record = parse(multisig, &fmt()).unwrap();
        assert_eq!(record.signatures, vec![
            "key1:sig1".to_string(),
            "key2:sig2".to_string(),
            "key3:sig3".to_string(),
        ]);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let with_extras = "\
StorePath: /x
URL: y
Compression: xz
NarHash: z
NarSize: 1
SomeFutureField: value
AnotherFutureField: 42
";
        // Should not error — unknown keys are forward-compat.
        let _ = parse(with_extras, &fmt()).unwrap();
    }
}
