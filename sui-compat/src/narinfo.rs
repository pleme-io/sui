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
    pub fn serialize(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("StorePath: {}\n", self.store_path));
        out.push_str(&format!("URL: {}\n", self.url));
        out.push_str(&format!("Compression: {}\n", self.compression));
        out.push_str(&format!("FileHash: {}\n", self.file_hash));
        out.push_str(&format!("FileSize: {}\n", self.file_size));
        out.push_str(&format!("NarHash: {}\n", self.nar_hash));
        out.push_str(&format!("NarSize: {}\n", self.nar_size));
        out.push_str(&format!("References: {}\n", self.references.join(" ")));
        if let Some(ref d) = self.deriver {
            out.push_str(&format!("Deriver: {d}\n"));
        }
        for sig in &self.signatures {
            out.push_str(&format!("Sig: {sig}\n"));
        }
        if let Some(ref ca) = self.ca {
            out.push_str(&format!("CA: {ca}\n"));
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
}
