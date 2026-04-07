//! NarInfo binary cache metadata format.
//!
//! Clean-room implementation from the NarInfo specification.
//! Key-value format, one field per line. Content-Type: `text/x-nix-narinfo`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NarInfoError {
    #[error("missing required field: {0}")]
    MissingField(String),
    #[error("invalid field value for {field}: {value}")]
    InvalidValue { field: String, value: String },
    #[error("parse error: {0}")]
    Parse(String),
}

/// Parsed NarInfo metadata for a store path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarInfo {
    /// Full store path (e.g., `/nix/store/abc...-hello-2.12.1`).
    pub store_path: String,
    /// Relative URL to the NAR file.
    pub url: String,
    /// Compression method (`xz`, `zstd`, `none`).
    pub compression: String,
    /// Hash of the compressed NAR file (`sha256:<hex>`).
    pub file_hash: String,
    /// Size of the compressed NAR file in bytes.
    pub file_size: u64,
    /// Hash of the uncompressed NAR (`sha256:<hex>`).
    pub nar_hash: String,
    /// Size of the uncompressed NAR in bytes.
    pub nar_size: u64,
    /// Runtime dependency store path basenames (space-separated in wire format).
    pub references: Vec<String>,
    /// `.drv` basename that built this path.
    pub deriver: Option<String>,
    /// Ed25519 signatures (`keyname:base64sig`).
    pub signatures: Vec<String>,
    /// Content-address assertion (if applicable).
    pub ca: Option<String>,
}

impl std::fmt::Display for NarInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.serialize())
    }
}

impl std::str::FromStr for NarInfo {
    type Err = NarInfoError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl NarInfo {
    /// Parse a NarInfo from its text representation.
    pub fn parse(input: &str) -> Result<Self, NarInfoError> {
        let mut store_path = None;
        let mut url = None;
        let mut compression = None;
        let mut file_hash = None;
        let mut file_size = None;
        let mut nar_hash = None;
        let mut nar_size = None;
        let mut references = Vec::new();
        let mut deriver = None;
        let mut signatures = Vec::new();
        let mut ca = None;

        for line in input.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| NarInfoError::Parse(format!("no colon in line: {line}")))?;
            let key = key.trim();
            let value = value.trim();

            match key {
                "StorePath" => store_path = Some(value.to_string()),
                "URL" => url = Some(value.to_string()),
                "Compression" => compression = Some(value.to_string()),
                "FileHash" => file_hash = Some(value.to_string()),
                "FileSize" => {
                    file_size = Some(value.parse::<u64>().map_err(|_| NarInfoError::InvalidValue {
                        field: "FileSize".to_string(),
                        value: value.to_string(),
                    })?);
                }
                "NarHash" => nar_hash = Some(value.to_string()),
                "NarSize" => {
                    nar_size = Some(value.parse::<u64>().map_err(|_| NarInfoError::InvalidValue {
                        field: "NarSize".to_string(),
                        value: value.to_string(),
                    })?);
                }
                "References" => {
                    if !value.is_empty() {
                        references = value.split_whitespace().map(String::from).collect();
                    }
                }
                "Deriver" => {
                    if !value.is_empty() {
                        deriver = Some(value.to_string());
                    }
                }
                "Sig" => signatures.push(value.to_string()),
                "CA" => ca = Some(value.to_string()),
                _ => {} // Ignore unknown fields
            }
        }

        Ok(NarInfo {
            store_path: store_path.ok_or_else(|| NarInfoError::MissingField("StorePath".to_string()))?,
            url: url.ok_or_else(|| NarInfoError::MissingField("URL".to_string()))?,
            compression: compression.unwrap_or_else(|| "none".to_string()),
            file_hash: file_hash.ok_or_else(|| NarInfoError::MissingField("FileHash".to_string()))?,
            file_size: file_size.ok_or_else(|| NarInfoError::MissingField("FileSize".to_string()))?,
            nar_hash: nar_hash.ok_or_else(|| NarInfoError::MissingField("NarHash".to_string()))?,
            nar_size: nar_size.ok_or_else(|| NarInfoError::MissingField("NarSize".to_string()))?,
            references,
            deriver,
            signatures,
            ca,
        })
    }

    /// Serialize to the NarInfo text format.
    #[must_use]
    pub fn serialize(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let _ = writeln!(out, "StorePath: {}", self.store_path);
        let _ = writeln!(out, "URL: {}", self.url);
        let _ = writeln!(out, "Compression: {}", self.compression);
        let _ = writeln!(out, "FileHash: {}", self.file_hash);
        let _ = writeln!(out, "FileSize: {}", self.file_size);
        let _ = writeln!(out, "NarHash: {}", self.nar_hash);
        let _ = writeln!(out, "NarSize: {}", self.nar_size);
        let _ = writeln!(out, "References: {}", self.references.join(" "));
        if let Some(ref d) = self.deriver {
            let _ = writeln!(out, "Deriver: {d}");
        }
        for sig in &self.signatures {
            let _ = writeln!(out, "Sig: {sig}");
        }
        if let Some(ref ca) = self.ca {
            let _ = writeln!(out, "CA: {ca}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_NARINFO: &str = "\
StorePath: /nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
URL: nar/1nhgq6wcggx0plpy4991h3ginj6hipsdslv4fd5eqbhwc1q8ydsn.nar.xz
Compression: xz
FileHash: sha256:0d6cc2d69a89a98d02b21e7b725e3c2a4d3eec166ccaee16f14dc67c3a8c6cd0
FileSize: 42856
NarHash: sha256:1b0ri5lsf45dknj8bfxi1syz35kmab77apxxg1yrf33la1qm3kc7
NarSize: 226552
References: 3n58xw4373jp0ljirf06d8077j15pc4j-glibc-2.37-8 sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1
Deriver: xb4y5iklhya4blk42k1cfkb8k07dpp4n-hello-2.12.1.drv
Sig: cache.nixos.org-1:8ijECciSFzWHwwGVOIVYdp2fOIOJAfgKhsVlKj/trdKJAjQMEhWNNBAPJRnlBNzA7buqPhLox5NW3S0EKgqICw==
";

    #[test]
    fn parse_sample() {
        let info = NarInfo::parse(SAMPLE_NARINFO).unwrap();
        assert_eq!(info.store_path, "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1");
        assert_eq!(info.compression, "xz");
        assert_eq!(info.file_size, 42856);
        assert_eq!(info.nar_size, 226552);
        assert_eq!(info.references.len(), 2);
        assert!(info.deriver.is_some());
        assert_eq!(info.signatures.len(), 1);
    }

    #[test]
    fn serialize_roundtrip() {
        let info = NarInfo::parse(SAMPLE_NARINFO).unwrap();
        let serialized = info.serialize();
        let reparsed = NarInfo::parse(&serialized).unwrap();
        assert_eq!(info, reparsed);
    }

    #[test]
    fn missing_store_path() {
        let input = "URL: nar/foo.nar\nNarHash: sha256:abc\nNarSize: 100\nFileHash: sha256:def\nFileSize: 50\n";
        assert!(NarInfo::parse(input).is_err());
    }

    #[test]
    fn no_references() {
        let input = "\
StorePath: /nix/store/abc-foo
URL: nar/foo.nar
Compression: none
FileHash: sha256:abc
FileSize: 100
NarHash: sha256:def
NarSize: 200
References: \n";
        let info = NarInfo::parse(input).unwrap();
        assert!(info.references.is_empty());
    }

    #[test]
    fn multiple_signatures() {
        let input = "\
StorePath: /nix/store/abc-foo
URL: nar/foo.nar
Compression: none
FileHash: sha256:abc
FileSize: 100
NarHash: sha256:def
NarSize: 200
References: \n\
Sig: key1:aaa==
Sig: key2:bbb==\n";
        let info = NarInfo::parse(input).unwrap();
        assert_eq!(info.signatures.len(), 2);
        assert_eq!(info.signatures[0], "key1:aaa==");
        assert_eq!(info.signatures[1], "key2:bbb==");
    }

    #[test]
    fn narinfo_with_content_address() {
        let input = "\
StorePath: /nix/store/abc-source.tar.gz
URL: nar/abc.nar.xz
Compression: xz
FileHash: sha256:1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef
FileSize: 5000
NarHash: sha256:abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890
NarSize: 10000
References: \n\
CA: fixed:out:r:sha256:deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n";
        let info = NarInfo::parse(input).unwrap();
        assert_eq!(info.ca, Some("fixed:out:r:sha256:deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string()));

        // Roundtrip preserves CA
        let serialized = info.serialize();
        let reparsed = NarInfo::parse(&serialized).unwrap();
        assert_eq!(reparsed.ca, info.ca);
    }

    #[test]
    fn narinfo_with_no_deriver() {
        let input = "\
StorePath: /nix/store/abc-foo
URL: nar/foo.nar
Compression: none
FileHash: sha256:abc
FileSize: 100
NarHash: sha256:def
NarSize: 200
References: \n";
        let info = NarInfo::parse(input).unwrap();
        assert!(info.deriver.is_none());

        // Serialized output should not contain Deriver line
        let serialized = info.serialize();
        assert!(!serialized.contains("Deriver:"));
    }

    #[test]
    fn narinfo_with_empty_references() {
        let input = "\
StorePath: /nix/store/abc-foo
URL: nar/foo.nar
Compression: none
FileHash: sha256:abc
FileSize: 100
NarHash: sha256:def
NarSize: 200
References: \n";
        let info = NarInfo::parse(input).unwrap();
        assert!(info.references.is_empty());
    }

    #[test]
    fn unknown_fields_ignored() {
        let input = "\
StorePath: /nix/store/abc-foo
URL: nar/foo.nar
Compression: none
FileHash: sha256:abc
FileSize: 100
NarHash: sha256:def
NarSize: 200
References: \n\
FutureField: some-value
AnotherUnknown: 42\n";
        let info = NarInfo::parse(input).unwrap();
        assert_eq!(info.store_path, "/nix/store/abc-foo");
        // Unknown fields should not cause errors
    }

    #[test]
    fn missing_url_field() {
        let input = "\
StorePath: /nix/store/abc-foo
Compression: none
FileHash: sha256:abc
FileSize: 100
NarHash: sha256:def
NarSize: 200
References: \n";
        match NarInfo::parse(input) {
            Err(NarInfoError::MissingField(f)) => assert_eq!(f, "URL"),
            other => panic!("expected MissingField(URL), got {other:?}"),
        }
    }

    #[test]
    fn missing_nar_hash_field() {
        let input = "\
StorePath: /nix/store/abc-foo
URL: nar/foo.nar
Compression: none
FileHash: sha256:abc
FileSize: 100
NarSize: 200
References: \n";
        match NarInfo::parse(input) {
            Err(NarInfoError::MissingField(f)) => assert_eq!(f, "NarHash"),
            other => panic!("expected MissingField(NarHash), got {other:?}"),
        }
    }

    #[test]
    fn missing_file_hash_field() {
        let input = "\
StorePath: /nix/store/abc-foo
URL: nar/foo.nar
Compression: none
FileSize: 100
NarHash: sha256:def
NarSize: 200
References: \n";
        match NarInfo::parse(input) {
            Err(NarInfoError::MissingField(f)) => assert_eq!(f, "FileHash"),
            other => panic!("expected MissingField(FileHash), got {other:?}"),
        }
    }

    #[test]
    fn invalid_file_size_value() {
        let input = "\
StorePath: /nix/store/abc-foo
URL: nar/foo.nar
FileHash: sha256:abc
FileSize: not-a-number
NarHash: sha256:def
NarSize: 200
References: \n";
        match NarInfo::parse(input) {
            Err(NarInfoError::InvalidValue { field, .. }) => assert_eq!(field, "FileSize"),
            other => panic!("expected InvalidValue for FileSize, got {other:?}"),
        }
    }

    #[test]
    fn default_compression_when_missing() {
        let input = "\
StorePath: /nix/store/abc-foo
URL: nar/foo.nar
FileHash: sha256:abc
FileSize: 100
NarHash: sha256:def
NarSize: 200
References: \n";
        let info = NarInfo::parse(input).unwrap();
        assert_eq!(info.compression, "none");
    }

    // ── Construct a fully-populated NarInfo for additional tests ──

    fn make_full_narinfo() -> NarInfo {
        NarInfo {
            store_path: "/nix/store/abc-pkg".to_string(),
            url: "nar/abc.nar.xz".to_string(),
            compression: "xz".to_string(),
            file_hash: "sha256:1111".to_string(),
            file_size: 1234,
            nar_hash: "sha256:2222".to_string(),
            nar_size: 5678,
            references: vec!["dep1".to_string(), "dep2".to_string()],
            deriver: Some("def-pkg.drv".to_string()),
            signatures: vec!["k1:s1".to_string(), "k2:s2".to_string()],
            ca: Some("fixed:out:r:sha256:cafe".to_string()),
        }
    }

    // ── Display + FromStr ────────────────────────────────

    #[test]
    fn display_trait_matches_serialize() {
        let info = make_full_narinfo();
        let s = format!("{info}");
        assert_eq!(s, info.serialize());
    }

    #[test]
    fn from_str_trait_matches_parse() {
        use std::str::FromStr;
        let info = make_full_narinfo();
        let s = info.serialize();
        let parsed = NarInfo::from_str(&s).unwrap();
        assert_eq!(parsed, info);
    }

    // ── Roundtrip preserves every field ──────────────────

    #[test]
    fn full_roundtrip_preserves_every_field() {
        let info = make_full_narinfo();
        let s = info.serialize();
        let parsed = NarInfo::parse(&s).unwrap();
        assert_eq!(parsed.store_path, info.store_path);
        assert_eq!(parsed.url, info.url);
        assert_eq!(parsed.compression, info.compression);
        assert_eq!(parsed.file_hash, info.file_hash);
        assert_eq!(parsed.file_size, info.file_size);
        assert_eq!(parsed.nar_hash, info.nar_hash);
        assert_eq!(parsed.nar_size, info.nar_size);
        assert_eq!(parsed.references, info.references);
        assert_eq!(parsed.deriver, info.deriver);
        assert_eq!(parsed.signatures, info.signatures);
        assert_eq!(parsed.ca, info.ca);
    }

    // ── Missing required field variants ──────────────────

    #[test]
    fn missing_file_size_field() {
        let input = "\
StorePath: /nix/store/abc-foo
URL: nar/foo.nar
FileHash: sha256:abc
NarHash: sha256:def
NarSize: 200
References: \n";
        match NarInfo::parse(input) {
            Err(NarInfoError::MissingField(f)) => assert_eq!(f, "FileSize"),
            other => panic!("expected MissingField(FileSize), got {other:?}"),
        }
    }

    #[test]
    fn missing_nar_size_field() {
        let input = "\
StorePath: /nix/store/abc-foo
URL: nar/foo.nar
FileHash: sha256:abc
FileSize: 100
NarHash: sha256:def
References: \n";
        match NarInfo::parse(input) {
            Err(NarInfoError::MissingField(f)) => assert_eq!(f, "NarSize"),
            other => panic!("expected MissingField(NarSize), got {other:?}"),
        }
    }

    #[test]
    fn invalid_nar_size_value() {
        let input = "\
StorePath: /nix/store/abc-foo
URL: nar/foo.nar
FileHash: sha256:abc
FileSize: 100
NarHash: sha256:def
NarSize: not-numeric
References: \n";
        match NarInfo::parse(input) {
            Err(NarInfoError::InvalidValue { field, value }) => {
                assert_eq!(field, "NarSize");
                assert_eq!(value, "not-numeric");
            }
            other => panic!("expected InvalidValue for NarSize, got {other:?}"),
        }
    }

    #[test]
    fn parse_error_no_colon_in_line() {
        let input = "this-is-not-a-key-value-line\n";
        match NarInfo::parse(input) {
            Err(NarInfoError::Parse(_)) => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn empty_input_returns_missing_field() {
        // Empty input → first missing required field is StorePath
        match NarInfo::parse("") {
            Err(NarInfoError::MissingField(f)) => assert_eq!(f, "StorePath"),
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[test]
    fn blank_lines_ignored() {
        let input = "\
\n\
\n\
StorePath: /nix/store/abc-foo\n\
\n\
URL: nar/foo.nar\n\
\n\
Compression: none\n\
FileHash: sha256:abc\n\
FileSize: 100\n\
NarHash: sha256:def\n\
NarSize: 200\n\
References: \n";
        let info = NarInfo::parse(input).unwrap();
        assert_eq!(info.store_path, "/nix/store/abc-foo");
    }

    #[test]
    fn references_split_on_whitespace() {
        let input = "\
StorePath: /nix/store/abc-foo
URL: nar/foo.nar
Compression: none
FileHash: sha256:abc
FileSize: 100
NarHash: sha256:def
NarSize: 200
References: a-1 b-2 c-3 d-4 e-5\n";
        let info = NarInfo::parse(input).unwrap();
        assert_eq!(info.references.len(), 5);
        assert_eq!(info.references[0], "a-1");
        assert_eq!(info.references[4], "e-5");
    }

    #[test]
    fn three_signatures_preserved_in_order() {
        let mut info = NarInfo::parse(SAMPLE_NARINFO).unwrap();
        info.signatures = vec![
            "k1:s1".to_string(),
            "k2:s2".to_string(),
            "k3:s3".to_string(),
        ];
        let s = info.serialize();
        let parsed = NarInfo::parse(&s).unwrap();
        assert_eq!(parsed.signatures, info.signatures);
    }

    #[test]
    fn deriver_field_serializes_when_some() {
        let mut info = NarInfo::parse(SAMPLE_NARINFO).unwrap();
        info.deriver = Some("xyz.drv".to_string());
        let s = info.serialize();
        assert!(s.contains("Deriver: xyz.drv"));
    }

    #[test]
    fn ca_field_serializes_when_some() {
        let mut info = NarInfo::parse(SAMPLE_NARINFO).unwrap();
        info.ca = Some("text:sha256:1234".to_string());
        let s = info.serialize();
        assert!(s.contains("CA: text:sha256:1234"));
    }

    #[test]
    fn ca_field_omitted_when_none() {
        let mut info = NarInfo::parse(SAMPLE_NARINFO).unwrap();
        info.ca = None;
        let s = info.serialize();
        assert!(!s.contains("CA:"));
    }

    #[test]
    fn deriver_field_omitted_when_none() {
        let mut info = NarInfo::parse(SAMPLE_NARINFO).unwrap();
        info.deriver = None;
        let s = info.serialize();
        assert!(!s.contains("Deriver:"));
    }

    #[test]
    fn maximum_file_size_value() {
        let input = format!(
            "StorePath: /nix/store/abc-foo\n\
URL: nar/foo.nar\n\
Compression: none\n\
FileHash: sha256:abc\n\
FileSize: {}\n\
NarHash: sha256:def\n\
NarSize: {}\n\
References: \n",
            u64::MAX,
            u64::MAX,
        );
        let info = NarInfo::parse(&input).unwrap();
        assert_eq!(info.file_size, u64::MAX);
        assert_eq!(info.nar_size, u64::MAX);
    }

    #[test]
    fn references_dedup_preserved_order() {
        let mut info = NarInfo::parse(SAMPLE_NARINFO).unwrap();
        info.references = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        // serialize doesn't dedup; round-trip preserves duplicates
        let s = info.serialize();
        let parsed = NarInfo::parse(&s).unwrap();
        assert_eq!(parsed.references, vec!["a".to_string(), "b".to_string(), "a".to_string()]);
    }

    #[test]
    fn whitespace_in_field_values_trimmed() {
        let input = "\
StorePath:    /nix/store/abc-foo
URL:    nar/foo.nar
Compression:    none
FileHash:    sha256:abc
FileSize:    100
NarHash:    sha256:def
NarSize:    200
References:    \n";
        let info = NarInfo::parse(input).unwrap();
        assert_eq!(info.store_path, "/nix/store/abc-foo");
        assert_eq!(info.url, "nar/foo.nar");
    }

    #[test]
    fn signature_with_complex_base64_preserved() {
        let sig = "cache.example.org-1:ABCDEFG+/=Hijklmn==";
        let mut info = NarInfo::parse(SAMPLE_NARINFO).unwrap();
        info.signatures = vec![sig.to_string()];
        let parsed = NarInfo::parse(&info.serialize()).unwrap();
        assert_eq!(parsed.signatures, vec![sig.to_string()]);
    }

    #[test]
    fn url_with_query_string_preserved() {
        let mut info = NarInfo::parse(SAMPLE_NARINFO).unwrap();
        info.url = "nar/abc.nar.xz?token=xyz".to_string();
        let parsed = NarInfo::parse(&info.serialize()).unwrap();
        assert_eq!(parsed.url, "nar/abc.nar.xz?token=xyz");
    }

    #[test]
    fn store_path_with_long_name_preserved() {
        let mut info = NarInfo::parse(SAMPLE_NARINFO).unwrap();
        let long = format!("/nix/store/{}-{}", "a".repeat(32), "x".repeat(200));
        info.store_path = long.clone();
        let parsed = NarInfo::parse(&info.serialize()).unwrap();
        assert_eq!(parsed.store_path, long);
    }

    #[test]
    fn many_references_preserved() {
        let mut info = NarInfo::parse(SAMPLE_NARINFO).unwrap();
        info.references = (0..50).map(|i| format!("ref-{i:03}-name")).collect();
        let parsed = NarInfo::parse(&info.serialize()).unwrap();
        assert_eq!(parsed.references.len(), 50);
        assert_eq!(parsed.references[0], "ref-000-name");
        assert_eq!(parsed.references[49], "ref-049-name");
    }
}
