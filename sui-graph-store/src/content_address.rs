//! BLAKE3-based content addressing.
//!
//! Every typed graph is identified by the BLAKE3 of its canonical rkyv
//! archive bytes. Because rkyv 0.8's wire format is deterministic for
//! a given type+value, this hash is **stable across machines and
//! across crate versions within the 0.8 series**.
//!
//! ## Encoding
//!
//! Displayed and serialized as **Nix-style base32** (the 32-character
//! alphabet `0-9 a-z` minus `e`, `o`, `t`, `u` — same as `nix-hash`)
//! truncated to the conventional 52-character store-path digest length.
//! This makes hashes look right at home next to existing Nix store paths
//! and shortens filesystem path components below most operating-system
//! `NAME_MAX` limits (255 bytes) with comfortable headroom.
//!
//! Internally the hash is the full 32-byte BLAKE3 digest; only the
//! display form is truncated.

use std::fmt;
use std::str::FromStr;

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};

use crate::error::Error;

/// 32-byte BLAKE3 content hash. The canonical identity of every graph
/// in the store.
///
/// Carries rkyv `Archive` derives so wire types in `sui-protocol`
/// (and any future archived form referring to a stored blob) can embed
/// it cheaply. The archived form is a fixed-32-byte fixed-layout, so
/// zero-copy reads of a `GraphHash` are a literal pointer offset.
#[derive(
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
)]
#[rkyv(derive(Debug, PartialEq, Eq))]
pub struct GraphHash(pub [u8; 32]);

impl GraphHash {
    /// Compute the BLAKE3 of an arbitrary byte slice. This is the
    /// canonical "what's the hash of these archive bytes" entry point —
    /// callers compute it once at archive time and again at retrieve
    /// time to verify.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).into())
    }

    /// Raw 32-byte form. Useful when threading into a fixed-width
    /// keying surface (redb key, protobuf bytes field, etc.).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Truncated display form (52 base32 chars, matches Nix store
    /// digest length). Used in file names and log lines.
    #[must_use]
    pub fn display_short(&self) -> String {
        let full = self.to_string();
        full.chars().take(52).collect()
    }

    /// Two-byte/two-byte fan-out prefix for the on-disk path. Returns
    /// `(aa, bb)` where `aa` and `bb` are the first two and second two
    /// hex characters of the hash — chosen so the fan-out spans
    /// 256 × 256 = 65 536 leaf directories without ever stat-ing more
    /// than ~16 blobs per leaf (Git's proven shape).
    #[must_use]
    pub fn shard_prefix(&self) -> (String, String) {
        let hex = data_encoding::HEXLOWER.encode(&self.0[..2]);
        (hex[0..2].to_string(), hex[2..4].to_string())
    }
}

impl fmt::Debug for GraphHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GraphHash({})", self.display_short())
    }
}

impl fmt::Display for GraphHash {
    /// Nix-style base32 of the full 32-byte digest. Stable across runs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = nix_base32_encode(&self.0);
        f.write_str(&encoded)
    }
}

impl FromStr for GraphHash {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = nix_base32_decode(s).ok_or(Error::BadHash {
            input: s.to_string(),
            reason: "not a valid nix-base32 encoding",
        })?;
        if bytes.len() != 32 {
            return Err(Error::BadHash {
                input: s.to_string(),
                reason: "wrong digest length (expected 32 bytes)",
            });
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(Self(out))
    }
}

// ── Nix-style base32 codec ───────────────────────────────────────────
// Mirrors `nix-hash --to-base32` exactly. Alphabet: 0-9, a-z minus e,o,t,u.
// Bit packing is LSB-first across the input bytes; output length is
// `(len * 8 + 4) / 5` characters. This is the same routine cppnix uses.

const NIX_BASE32_CHARS: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

fn nix_base32_encode(bytes: &[u8]) -> String {
    let out_len = (bytes.len() * 8 + 4) / 5;
    let mut out = String::with_capacity(out_len);
    for i in (0..out_len).rev() {
        let b = i * 5;
        let byte = b / 8;
        let bit = b % 8;
        let mut c = u16::from(bytes[byte]) >> bit;
        if byte + 1 < bytes.len() {
            c |= u16::from(bytes[byte + 1]) << (8 - bit);
        }
        out.push(NIX_BASE32_CHARS[(c & 0x1f) as usize] as char);
    }
    out
}

fn nix_base32_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    let bytes_len = s.len() * 5 / 8;
    let mut out = vec![0u8; bytes_len];
    for (i, &c) in s.iter().rev().enumerate() {
        let digit = NIX_BASE32_CHARS.iter().position(|&x| x == c)?;
        let b = i * 5;
        let byte = b / 8;
        let bit = b % 8;
        let lo = (digit as u16) << bit;
        let hi = (digit as u16) >> (8 - bit);
        out[byte] |= (lo & 0xff) as u8;
        if hi != 0 && byte + 1 < bytes_len {
            out[byte + 1] |= hi as u8;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn hash_of_empty_is_blake3_empty() {
        let h = GraphHash::of(&[]);
        // BLAKE3 of empty input is a known constant.
        let expected: [u8; 32] = [
            0xaf, 0x13, 0x49, 0xb9, 0xf5, 0xf9, 0xa1, 0xa6, 0xa0, 0x40, 0x4d, 0xea, 0x36, 0xdc,
            0xc9, 0x49, 0x9b, 0xcb, 0x25, 0xc9, 0xad, 0xc1, 0x12, 0xb7, 0xcc, 0x9a, 0x93, 0xca,
            0xe4, 0x1f, 0x32, 0x62,
        ];
        assert_eq!(h.as_bytes(), &expected);
    }

    #[test]
    fn base32_roundtrip() {
        for input in [b"".as_slice(), b"hello", b"sui graph store"] {
            // BLAKE3 to get exactly 32 bytes.
            let h = GraphHash::of(input);
            let s = h.to_string();
            let back: GraphHash = s.parse().unwrap();
            assert_eq!(h, back);
        }
    }

    #[test]
    fn shard_prefix_is_two_two_hex() {
        let h = GraphHash::of(b"shard test");
        let (a, b) = h.shard_prefix();
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(b.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn display_short_is_52_chars() {
        let h = GraphHash::of(b"display test");
        assert_eq!(h.display_short().len(), 52);
    }

    #[test]
    fn bad_hash_string_returns_error() {
        let r: Result<GraphHash, _> = "not a real hash!@#".parse();
        assert!(r.is_err());
    }
}
