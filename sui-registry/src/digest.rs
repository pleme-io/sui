//! The typed OCI content-address: [`Digest`] and [`Reference`].
//!
//! **Parse-at-boundary is the whole point.** An OCI digest is
//! `<algorithm>:<hex>` where the algorithm is `sha256` or `sha512` and the
//! hex is exactly `2 * digest_len` lowercase hex characters. A malformed
//! digest string has *no* path to a constructed [`Digest`] — the only
//! ingress is [`Digest::parse`], which rejects it as
//! [`DigestError`] (rendered to the wire as `DIGEST_INVALID`). Downstream
//! code that holds a `Digest` therefore holds a *proof* the address is
//! well-formed; a wrong-shaped digest is unrepresentable past the border.
//!
//! We reuse [`sui_compat::hash::HashAlgorithm`] for the algorithm axis (the
//! substrate already owns Nix's hash-algorithm vocabulary) but constrain the
//! set to exactly the two the OCI spec blesses — `sha256` (registry MUST) and
//! `sha512` (registry MAY). `sha1`/`md5` are not OCI content-address
//! algorithms and are rejected at the border, not merely unused.

use std::fmt;

use sui_compat::hash::HashAlgorithm;

/// A parse-time rejection of a malformed content-address.
///
/// Every variant maps to the OCI `DIGEST_INVALID` wire code (see
/// [`crate::error::OciError`]); the distinct variants exist so the *server
/// operator's* logs name the exact malformation, while the *client* only ever
/// sees one typed wire error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DigestError {
    /// No `:` separating the algorithm from the hex encoding.
    #[error("digest missing '<algorithm>:<hex>' separator")]
    MissingSeparator,
    /// The algorithm component is not an OCI content-address algorithm.
    #[error("unsupported digest algorithm: {0} (want sha256 or sha512)")]
    UnsupportedAlgorithm(String),
    /// The hex encoding is not the exact length the algorithm requires, or
    /// contains a non-lowercase-hex character.
    #[error("malformed digest hex encoding")]
    MalformedHex,
}

/// A well-formed OCI content-address: an algorithm plus its lowercase-hex
/// encoding.
///
/// Construction is only via [`Digest::parse`] (parse-at-boundary) or
/// [`Digest::of`] (hash real bytes) — both of which *guarantee* the invariant.
/// There is no public field-wise constructor, so a `Digest` in hand is
/// always well-formed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest {
    algo: HashAlgorithm,
    /// Lowercase-hex encoding of the raw digest, exactly `2 * digest_len` chars.
    hex: String,
}

impl Digest {
    /// Parse `<algorithm>:<hex>` at the trust boundary.
    ///
    /// The ONLY string ingress for a [`Digest`]. Rejects anything that is not
    /// `sha256`/`sha512` with an exact-length lowercase-hex body.
    ///
    /// # Errors
    ///
    /// [`DigestError`] on a missing separator, an unsupported algorithm, or a
    /// wrong-length / non-lowercase-hex body.
    pub fn parse(raw: &str) -> Result<Self, DigestError> {
        let (algo_str, hex) = raw.split_once(':').ok_or(DigestError::MissingSeparator)?;
        let algo = match algo_str {
            "sha256" => HashAlgorithm::Sha256,
            "sha512" => HashAlgorithm::Sha512,
            other => return Err(DigestError::UnsupportedAlgorithm(other.to_string())),
        };
        let want_len = algo.digest_len() * 2;
        if hex.len() != want_len {
            return Err(DigestError::MalformedHex);
        }
        // Lowercase hex only — OCI requires the canonical form; an uppercase
        // or non-hex byte is a distinct address string and must not alias.
        if !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(DigestError::MalformedHex);
        }
        Ok(Self { algo, hex: hex.to_string() })
    }

    /// Content-address `bytes` under `algo`, yielding a well-formed [`Digest`].
    ///
    /// This is the server-side digest computation used to *verify* an upload:
    /// hash the received bytes, compare the resulting [`Digest`] to the one the
    /// client claimed. Equality is structural (`==` over algo + hex).
    #[must_use]
    pub fn of(algo: HashAlgorithm, bytes: &[u8]) -> Self {
        use sha2::{Digest as _, Sha256, Sha512};
        let raw = match algo {
            HashAlgorithm::Sha512 => Sha512::digest(bytes).to_vec(),
            // Any non-sha512 request hashes under sha256 — the OCI default and
            // the only other blessed algorithm. (`Digest` never holds sha1/md5.)
            _ => Sha256::digest(bytes).to_vec(),
        };
        let algo = if matches!(algo, HashAlgorithm::Sha512) {
            HashAlgorithm::Sha512
        } else {
            HashAlgorithm::Sha256
        };
        Self { algo, hex: data_encoding::HEXLOWER.encode(&raw) }
    }

    /// The algorithm component.
    #[must_use]
    pub fn algorithm(&self) -> HashAlgorithm {
        self.algo
    }

    /// The lowercase-hex encoding (no `algo:` prefix).
    #[must_use]
    pub fn hex(&self) -> &str {
        &self.hex
    }

    /// The canonical `<algorithm>:<hex>` wire string.
    ///
    /// This is also the durable storage key: an OCI blob/manifest is keyed by
    /// this exact string in the underlying [`crate::store::RegistryStore`], so
    /// content-addressed dedup is free and cross-repo mount is a pointer op.
    #[must_use]
    pub fn to_wire(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Typed render surface (Display impl) — never a free-form format! of
        // wire syntax elsewhere. `HashAlgorithm`'s own Display yields the
        // canonical nix/oci-shared lowercase name (`sha256`/`sha512`).
        write!(f, "{}:{}", self.algo, self.hex)
    }
}

/// A manifest reference: either a mutable tag pointer or an immutable digest.
///
/// OCI `<reference>` in `/v2/<name>/manifests/<reference>` is *either* a tag
/// (a human name that points at a manifest and may be re-pointed) *or* a digest
/// (the immutable content-address). Making this a sum type means a handler
/// cannot forget which it is holding — tag-vs-digest dispatch is exhaustive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    /// A mutable tag name (validated against the OCI tag grammar).
    Tag(String),
    /// An immutable content-address.
    Digest(Digest),
}

impl Reference {
    /// Parse a `<reference>` path component: try digest first (it always
    /// contains a `:`), else validate as a tag.
    ///
    /// # Errors
    ///
    /// [`DigestError`] only when the component *looks* like a digest (contains
    /// `:`) but is malformed. A syntactically-valid tag never errors here;
    /// tag-grammar rejection is the caller's [`crate::error::OciError`] concern
    /// (a bad tag on a *write* is `MANIFEST_INVALID`, on a *read* is
    /// `MANIFEST_UNKNOWN`) so this function stays a pure lexer.
    pub fn parse(raw: &str) -> Result<Self, DigestError> {
        if raw.contains(':') {
            Ok(Reference::Digest(Digest::parse(raw)?))
        } else {
            Ok(Reference::Tag(raw.to_string()))
        }
    }

    /// Whether `raw` satisfies the OCI tag grammar
    /// `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`.
    #[must_use]
    pub fn is_valid_tag(raw: &str) -> bool {
        if raw.is_empty() || raw.len() > 128 {
            return false;
        }
        let mut chars = raw.chars();
        let Some(first) = chars.next() else { return false };
        if !(first.is_ascii_alphanumeric() || first == '_') {
            return false;
        }
        raw.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    }
}

/// Whether `name` satisfies the OCI repository-name grammar: one or more
/// path-components of `[a-z0-9]+` joined by separators `.`/`_`/`__`/`-`, the
/// components joined by `/`. A simplified but spec-faithful check.
#[must_use]
pub fn is_valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    name.split('/').all(|component| {
        !component.is_empty()
            && component
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
            // A component may not start or end with a separator.
            && !component.starts_with(['.', '_', '-'])
            && !component.ends_with(['.', '_', '-'])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256_zero() -> String {
        format!("sha256:{}", "0".repeat(64))
    }

    #[test]
    fn parse_accepts_wellformed_sha256() {
        let d = Digest::parse(&sha256_zero()).unwrap();
        assert_eq!(d.algorithm(), HashAlgorithm::Sha256);
        assert_eq!(d.hex().len(), 64);
        assert_eq!(d.to_wire(), sha256_zero());
    }

    #[test]
    fn parse_accepts_wellformed_sha512() {
        let raw = format!("sha512:{}", "a".repeat(128));
        let d = Digest::parse(&raw).unwrap();
        assert_eq!(d.algorithm(), HashAlgorithm::Sha512);
        assert_eq!(d.hex().len(), 128);
    }

    #[test]
    fn parse_rejects_missing_separator() {
        assert_eq!(
            Digest::parse("sha256deadbeef"),
            Err(DigestError::MissingSeparator)
        );
    }

    #[test]
    fn parse_rejects_unsupported_algorithm() {
        // sha1 is a real Nix algo but NOT an OCI content-address algorithm.
        let raw = format!("sha1:{}", "0".repeat(40));
        assert!(matches!(
            Digest::parse(&raw),
            Err(DigestError::UnsupportedAlgorithm(_))
        ));
    }

    #[test]
    fn parse_rejects_wrong_length_hex() {
        assert_eq!(
            Digest::parse("sha256:dead"),
            Err(DigestError::MalformedHex)
        );
    }

    #[test]
    fn parse_rejects_uppercase_hex() {
        // OCI requires the canonical lowercase form; uppercase is a distinct
        // (and invalid) address that must NOT alias the lowercase one.
        let raw = format!("sha256:{}", "A".repeat(64));
        assert_eq!(Digest::parse(&raw), Err(DigestError::MalformedHex));
    }

    #[test]
    fn of_computes_verifiable_digest() {
        let d = Digest::of(HashAlgorithm::Sha256, b"hello world");
        // Known sha256("hello world").
        assert_eq!(
            d.to_wire(),
            "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        // And it round-trips through parse (the border proof).
        assert_eq!(Digest::parse(&d.to_wire()).unwrap(), d);
    }

    #[test]
    fn reference_parses_digest_vs_tag() {
        match Reference::parse(&sha256_zero()).unwrap() {
            Reference::Digest(_) => {}
            other => panic!("expected digest, got {other:?}"),
        }
        match Reference::parse("v1.0.0").unwrap() {
            Reference::Tag(t) => assert_eq!(t, "v1.0.0"),
            other => panic!("expected tag, got {other:?}"),
        }
    }

    #[test]
    fn reference_malformed_digest_errors() {
        // Looks like a digest (has ':') but is malformed → DigestError.
        assert!(Reference::parse("sha256:short").is_err());
    }

    #[test]
    fn tag_grammar() {
        assert!(Reference::is_valid_tag("latest"));
        assert!(Reference::is_valid_tag("v1.2.3-rc1"));
        assert!(Reference::is_valid_tag("_underscore-start"));
        assert!(!Reference::is_valid_tag(""));
        assert!(!Reference::is_valid_tag(".dotstart"));
        assert!(!Reference::is_valid_tag("-dashstart"));
        assert!(!Reference::is_valid_tag(&"x".repeat(129)));
    }

    #[test]
    fn name_grammar() {
        assert!(is_valid_name("library/hello"));
        assert!(is_valid_name("my-app.v2/sub_module"));
        assert!(is_valid_name("simple"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("Uppercase"));
        assert!(!is_valid_name("trailing/"));
        assert!(!is_valid_name(".leading"));
    }
}
