//! **Admission** — the one gate every narinfo passes before a cache stores it.
//!
//! # The invariant
//!
//! A cache entry exists only if its NAR is the bytes its narinfo describes and
//! its narinfo lists every store path those bytes name:
//!
//! - `FileHash`/`FileSize` match the compressed NAR the `URL:` names;
//! - `NarHash`/`NarSize` match the NAR once decoded under `Compression:`;
//! - `References` holds every store hash the NAR names (file bytes and symlink
//!   targets) that is a real store path in the caller's [`RefUniverse`].
//!
//! # Why it is a type
//!
//! Measured on cid, 2026-10-26: the cache served
//! `rust-stable-with-components` with `References:` empty although its NAR is
//! symlinks into eight store paths. Every consumer registered it with no
//! references, GC took `rust-std`, and builds failed "can't find crate for std"
//! (the same class as receipt #23, where 909 such narinfos were moved aside by
//! hand). Nine write paths could store a narinfo, and none checked.
//!
//! [`AdmittedNarInfo`] has no public constructor except [`admit`] (and
//! [`AdmittedNarInfo::with_signatures`], which may change only `Sig:` lines), and
//! [`StorageBackend::put_narinfo`](super::StorageBackend::put_narinfo) takes one.
//! The raw record verb needs a [`RecordWrite`] that only this crate can make.
//! So a write path that skips admission does not compile.
//!
//! # Tier honesty
//!
//! - *Truly unrepresentable* outside this crate: a stored narinfo that did not
//!   pass [`admit`]. Inside it, [`TieredBackend`](super::TieredBackend)
//!   promotion copies a record one tier already holds, and the
//!   `test-fixtures` feature builds an unchecked value for tests.
//! - What a [`RefUniverse`] does not know, admission cannot judge: a NAR naming
//!   a path the cache has not yet seen is admitted with that name unlisted.
//!   `cache audit` re-runs admission over the stored set and catches it later.
//! - A content-addressed path (`CA:` set) is not judged on references: a
//!   fixed-output build may name paths it does not depend on, and cppnix
//!   refuses references there, so the scan cannot decide.

use std::collections::BTreeSet;
use std::fmt;
use std::io::Write;

use async_trait::async_trait;
use futures::StreamExt;
use sha2::Digest;
use sui_compat::hash::{HashAlgorithm, NixHash};
use sui_compat::nar_compression::NarCompression;
use sui_compat::narinfo::NarInfo;
use sui_compat::store_path_scan::{hash_part, NarDigester};

use super::nar_refs;
use super::nar_stream::NarStream;
use super::StorageBackend;
use crate::StoreError;

/// Why a narinfo was refused admission. Each one names the entry's fault, so
/// the caller refuses that entry and lets its siblings proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The text is not a narinfo nix can read.
    Malformed(String),
    /// The key it is stored under is not its `StorePath:`'s hash.
    KeyMismatch {
        /// The key the caller wanted to store under.
        key: String,
        /// The `StorePath:` the text names.
        store_path: String,
    },
    /// `URL:` is not a relative path this store may address.
    UnaddressableUrl(String),
    /// `Compression:` names a codec sui cannot decode.
    UnsupportedCompression(String),
    /// The NAR `URL:` names is not there.
    NarMissing {
        /// The advertised URL.
        url: String,
    },
    /// The NAR does not decode under its declared codec.
    NarCorrupt {
        /// The advertised URL.
        url: String,
        /// What the decoder said.
        reason: String,
    },
    /// A hash field does not parse as sha256.
    UnreadableHash {
        /// `FileHash` or `NarHash`.
        field: &'static str,
        /// The value as written.
        value: String,
    },
    /// `FileSize` is not the compressed size.
    FileSizeMismatch {
        /// As declared.
        declared: u64,
        /// As measured.
        actual: u64,
    },
    /// `FileHash` is not the compressed bytes' sha256.
    FileHashMismatch {
        /// As declared.
        declared: String,
        /// As measured (nix32).
        actual: String,
    },
    /// `NarSize` is not the decoded size.
    NarSizeMismatch {
        /// As declared.
        declared: u64,
        /// As measured.
        actual: u64,
    },
    /// `NarHash` is not the decoded bytes' sha256.
    NarHashMismatch {
        /// As declared.
        declared: String,
        /// As measured (nix32).
        actual: String,
    },
    /// `URL:` is content-keyed (`nar/<nix32 sha256>.nar…`) but names other
    /// bytes than the NAR file: two builders' bytes would alias.
    NarKeyMismatch {
        /// The advertised URL.
        url: String,
        /// The compressed bytes' sha256 (nix32).
        actual: String,
    },
    /// The NAR names real store paths `References:` does not list.
    OmittedReferences {
        /// The store hashes named and not listed.
        hashes: Vec<String>,
    },
}

impl Refusal {
    /// A stable, machine-readable name for logs and metrics.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "malformed",
            Self::KeyMismatch { .. } => "key-mismatch",
            Self::UnaddressableUrl(_) => "unaddressable-url",
            Self::UnsupportedCompression(_) => "unsupported-compression",
            Self::NarMissing { .. } => "nar-missing",
            Self::NarCorrupt { .. } => "nar-corrupt",
            Self::UnreadableHash { .. } => "unreadable-hash",
            Self::FileSizeMismatch { .. } => "file-size-mismatch",
            Self::FileHashMismatch { .. } => "file-hash-mismatch",
            Self::NarSizeMismatch { .. } => "nar-size-mismatch",
            Self::NarHashMismatch { .. } => "nar-hash-mismatch",
            Self::NarKeyMismatch { .. } => "nar-key-mismatch",
            Self::OmittedReferences { .. } => "omitted-references",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(e) => write!(f, "narinfo is malformed: {e}"),
            Self::KeyMismatch { key, store_path } => {
                write!(f, "narinfo for {store_path} cannot be stored under key {key}")
            }
            Self::UnaddressableUrl(u) => write!(f, "URL {u:?} is not an addressable relative path"),
            Self::UnsupportedCompression(c) => write!(f, "Compression {c:?} is not one sui decodes"),
            Self::NarMissing { url } => write!(f, "the NAR at {url} is not in the cache (upload it first)"),
            Self::NarCorrupt { url, reason } => write!(f, "the NAR at {url} does not decode: {reason}"),
            Self::UnreadableHash { field, value } => write!(f, "{field} {value:?} is not a sha256 hash"),
            Self::FileSizeMismatch { declared, actual } => {
                write!(f, "FileSize {declared} but the NAR file is {actual} bytes")
            }
            Self::FileHashMismatch { declared, actual } => {
                write!(f, "FileHash {declared} but the NAR file hashes to sha256:{actual}")
            }
            Self::NarSizeMismatch { declared, actual } => {
                write!(f, "NarSize {declared} but the NAR is {actual} bytes")
            }
            Self::NarHashMismatch { declared, actual } => {
                write!(f, "NarHash {declared} but the NAR hashes to sha256:{actual}")
            }
            Self::NarKeyMismatch { url, actual } => {
                write!(f, "URL {url} names other bytes than the NAR file, which hashes to sha256:{actual}")
            }
            Self::OmittedReferences { hashes } => write!(
                f,
                "the NAR names {} store path(s) References does not list: {}",
                hashes.len(),
                hashes.join(" ")
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// Admission's outcome when it did not admit: the entry's fault, or the
/// store's.
#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    /// The entry is bad; refuse it.
    #[error("refused ({kind}): {0}", kind = .0.kind())]
    Refused(Refusal),
    /// The store could not be read; nothing is known about the entry.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<Refusal> for AdmissionError {
    fn from(r: Refusal) -> Self {
        Self::Refused(r)
    }
}

impl From<AdmissionError> for StoreError {
    fn from(e: AdmissionError) -> Self {
        match e {
            AdmissionError::Refused(r) => StoreError::Refused(r),
            AdmissionError::Store(s) => s,
        }
    }
}

/// The store paths admission may hold a NAR to: a hash the NAR names is an
/// omitted reference only when this universe says it is real.
#[async_trait]
pub trait RefUniverse: Send + Sync {
    /// Whether `hash` names a store path that exists here.
    ///
    /// # Errors
    ///
    /// When the universe cannot be read; admission then reports a store error,
    /// never a verdict.
    async fn is_real(&self, hash: &str) -> Result<bool, StoreError>;
}

#[async_trait]
impl RefUniverse for BTreeSet<String> {
    async fn is_real(&self, hash: &str) -> Result<bool, StoreError> {
        Ok(self.contains(hash))
    }
}

#[async_trait]
impl RefUniverse for std::collections::HashSet<String> {
    async fn is_real(&self, hash: &str) -> Result<bool, StoreError> {
        Ok(self.contains(hash))
    }
}

/// A cache as a universe: a hash is real when the cache holds a servable
/// narinfo for it.
pub struct CacheUniverse<'a>(pub &'a dyn StorageBackend);

#[async_trait]
impl RefUniverse for CacheUniverse<'_> {
    async fn is_real(&self, hash: &str) -> Result<bool, StoreError> {
        Ok(self.0.get_narinfo(hash).await?.is_some_and(|t| nar_refs::is_servable_narinfo(&t)))
    }
}

/// The permission to call a raw narinfo record verb. Only this crate can make
/// one, so only this crate's composed verbs can write a record.
///
/// Outside this crate it cannot be built, so a write path that skips
/// admission does not compile:
///
/// ```compile_fail,E0423
/// let _ = sui_castore::RecordWrite(());
/// ```
///
/// ```compile_fail,E0451
/// let _ = sui_castore::AdmittedNarInfo { key: String::new(), info: todo!(), text: String::new() };
/// ```
///
/// ```compile_fail,E0061
/// async fn write(s: &dyn sui_castore::StorageBackend) {
///     s.put_narinfo("hash", "StorePath: /nix/store/hash-x\n").await.unwrap();
/// }
/// ```
#[derive(Debug)]
pub struct RecordWrite(());

impl RecordWrite {
    pub(crate) fn mint() -> Self {
        Self(())
    }
}

/// A narinfo that passed [`admit`]: the only value
/// [`put_narinfo`](super::StorageBackend::put_narinfo) accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedNarInfo {
    key: String,
    info: NarInfo,
    text: String,
}

impl AdmittedNarInfo {
    /// The 32-char store hash this entry is stored under.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The parsed narinfo.
    #[must_use]
    pub fn info(&self) -> &NarInfo {
        &self.info
    }

    /// The text that is stored and served.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The same entry with its signatures replaced. Signatures are outside
    /// what admission checked, so this is the one edit that keeps the value
    /// admitted.
    #[must_use]
    pub fn with_signatures(&self, signatures: Vec<String>) -> Self {
        let info = NarInfo { signatures, ..self.info.clone() };
        Self { key: self.key.clone(), text: info.serialize(), info }
    }

    /// An entry built without admission, for tests that exercise storage
    /// mechanics rather than admission.
    #[cfg(any(test, feature = "test-fixtures"))]
    #[must_use]
    pub fn unchecked_fixture(key: &str, text: &str) -> Self {
        let info = NarInfo::parse(text).unwrap_or_else(|_| NarInfo {
            store_path: String::new(),
            url: String::new(),
            compression: String::new(),
            file_hash: String::new(),
            file_size: 0,
            nar_hash: String::new(),
            nar_size: 0,
            references: Vec::new(),
            deriver: None,
            signatures: Vec::new(),
            ca: None,
            provenance: None, extra: Vec::new(),
        });
        Self { key: key.to_string(), info, text: text.to_string() }
    }
}

/// The sha256 a content-keyed NAR path names: `nar/<52 nix32 chars>.nar…`,
/// the layout cppnix writes (`nar/<FileHash>.nar.xz`). `None` for any other
/// name, such as the store-hash-keyed `nar/<32 chars>.nar.zst` sui wrote
/// before, which stays readable.
///
/// A content-keyed name cannot be reused for other bytes, so two builders'
/// NARs for one store path never overwrite or alias each other.
#[must_use]
pub fn nar_content_key(nar_path: &str) -> Option<[u8; 32]> {
    let file = nar_path.strip_prefix("nar/")?;
    let (stem, ext) = file.split_once('.')?;
    if stem.len() != 52 || !ext.starts_with("nar") {
        return None;
    }
    let digest = sui_compat::hash::decode_hash_any(HashAlgorithm::Sha256, stem).ok()?;
    digest.try_into().ok()
}

/// The content-keyed NAR path for compressed bytes with sha256 `file_sha256`
/// and URL suffix `suffix` (`.nar`, `.nar.xz`, `.nar.zst`).
#[must_use]
pub fn content_keyed_nar_path(file_sha256: &[u8; 32], suffix: &str) -> String {
    format!("nar/{}{suffix}", sui_compat::store_path::nix_base32_encode(file_sha256))
}

/// Whether a stored narinfo is fit to serve under `key` without reading its
/// NAR: it parses, it names the store path `key` is, its URL is addressable
/// and its codec is one a client can decode. The serve-time half of
/// admission, cheap enough for every GET; a stored entry failing it is a miss.
#[must_use]
pub fn is_servable_under(key: &str, text: &str) -> bool {
    precheck(key, text).is_ok()
}

/// Replace the `Sig:` lines of the entry `storage` holds under `key`, and
/// nothing else, without reading its NAR.
///
/// The fast re-sign (`sui cache resign --signatures-only`): it cannot make an
/// entry worse than it was stored, but it also does not check it, so it
/// re-vouches for whatever is there. The default re-sign admits each entry
/// first. Returns `false` when there is no entry under `key`.
///
/// # Errors
///
/// [`StoreError::NarInfo`] when the stored text does not parse; otherwise the
/// read or the write.
pub async fn resign_stored(
    storage: &dyn StorageBackend,
    key: &str,
    signatures: impl FnOnce(&NarInfo) -> Vec<String> + Send,
) -> Result<bool, StoreError> {
    let Some(text) = storage.get_narinfo(key).await? else { return Ok(false) };
    let info = NarInfo::parse(&text).map_err(|e| StoreError::NarInfo(format!("{key}: {e}")))?;
    let signatures = signatures(&info);
    let info = NarInfo { signatures, ..info };
    super::write_narinfo(storage, key, &info.serialize()).await?;
    Ok(true)
}

/// Entries that pass admission, for tests in this and other crates.
#[cfg(any(test, feature = "test-fixtures"))]
pub mod fixture {
    use sui_compat::hash::{HashAlgorithm, NixHash};
    use sui_compat::nar::{NarNode, NarWriter};

    /// A store path's narinfo and the NAR file it advertises.
    #[derive(Debug, Clone)]
    pub struct FixtureEntry {
        /// The store hash (narinfo key).
        pub key: String,
        /// The narinfo text.
        pub text: String,
        /// The advertised `URL:`.
        pub url: String,
        /// The NAR file (`Compression: none`).
        pub nar_file: Vec<u8>,
    }

    /// A single-file store path `/nix/store/<key>-<name>` holding `contents`,
    /// listing `refs` (basenames) as references.
    #[must_use]
    pub fn entry(key: &str, name: &str, contents: &[u8], refs: &[&str]) -> FixtureEntry {
        let mut nar = Vec::new();
        NarWriter::write(&mut nar, &NarNode::Regular { executable: false, contents: contents.to_vec() })
            .expect("in-memory NAR");
        let h = NixHash::of(HashAlgorithm::Sha256, &nar).to_nix32();
        let url = format!("nar/{h}.nar");
        let text = format!(
            "StorePath: /nix/store/{key}-{name}\nURL: {url}\nCompression: none\nFileHash: sha256:{h}\n\
             FileSize: {len}\nNarHash: sha256:{h}\nNarSize: {len}\nReferences: {refs}\n",
            len = nar.len(),
            refs = refs.join(" "),
        );
        FixtureEntry { key: key.to_string(), text, url, nar_file: nar }
    }
}

/// Parse and check everything that needs no NAR bytes. Returns the narinfo
/// and its codec.
fn precheck(key: &str, text: &str) -> Result<(NarInfo, NarCompression), Refusal> {
    let info = NarInfo::parse(text).map_err(|e| Refusal::Malformed(e.to_string()))?;
    let named_key = info.store_path.rsplit('/').next().and_then(|b| b.split_once('-')).map(|(h, _)| h);
    if named_key != Some(key) || !info.store_path.starts_with('/') {
        return Err(Refusal::KeyMismatch { key: key.to_string(), store_path: info.store_path });
    }
    if !nar_refs::is_addressable_nar_path(&info.url) {
        return Err(Refusal::UnaddressableUrl(info.url));
    }
    let codec = NarCompression::from_narinfo_name(&info.compression)
        .ok_or_else(|| Refusal::UnsupportedCompression(info.compression.clone()))?;
    Ok((info, codec))
}

fn declared_sha256(field: &'static str, value: &str) -> Result<Vec<u8>, Refusal> {
    NixHash::parse_any(HashAlgorithm::Sha256, value)
        .map(|h| h.digest)
        .map_err(|_| Refusal::UnreadableHash { field, value: value.to_string() })
}

/// Admit `text` under `key`, reading the compressed NAR from `nar`.
///
/// `nar` is the NAR file the narinfo's `URL:` names (`None` when it is not
/// there). It is streamed once: the compressed bytes are hashed, decoded,
/// hashed again and scanned for store paths, and nothing is held whole.
///
/// # Errors
///
/// [`AdmissionError::Refused`] naming the entry's fault, or
/// [`AdmissionError::Store`] when the NAR stream or the universe failed to read.
pub async fn admit(
    key: &str,
    text: &str,
    nar: Option<NarStream>,
    universe: &dyn RefUniverse,
) -> Result<AdmittedNarInfo, AdmissionError> {
    let (info, codec) = precheck(key, text)?;
    let Some(mut nar) = nar else {
        return Err(Refusal::NarMissing { url: info.url }.into());
    };
    let corrupt = |e: std::io::Error| Refusal::NarCorrupt { url: info.url.clone(), reason: e.to_string() };

    let mut file_sha = sha2::Sha256::new();
    let mut file_size = 0u64;
    let mut decoder = codec.decoding_writer(NarDigester::new()).map_err(corrupt)?;
    while let Some(chunk) = nar.next().await {
        let chunk = chunk?;
        file_sha.update(&chunk);
        file_size += chunk.len() as u64;
        decoder.write_all(&chunk).map_err(corrupt)?;
    }
    let digest = decoder.finish().map_err(corrupt)?.finish();
    let file_sha: [u8; 32] = file_sha.finalize().into();

    judge(key, &info, file_size, &file_sha, &digest, universe).await?;
    Ok(AdmittedNarInfo { key: key.to_string(), text: info.serialize(), info })
}

/// Admit `text` under `key` against the NAR file bytes in hand.
///
/// # Errors
///
/// As [`admit`].
pub async fn admit_bytes(
    key: &str,
    text: &str,
    nar_file: &[u8],
    universe: &dyn RefUniverse,
) -> Result<AdmittedNarInfo, AdmissionError> {
    let stream = super::nar_stream::bytes_stream(bytes::Bytes::copy_from_slice(nar_file));
    admit(key, text, Some(stream), universe).await
}

/// Admit `text` under `key` against the NAR `storage` holds at its `URL:`.
///
/// # Errors
///
/// As [`admit`].
pub async fn admit_stored(
    storage: &dyn StorageBackend,
    key: &str,
    text: &str,
    universe: &dyn RefUniverse,
) -> Result<AdmittedNarInfo, AdmissionError> {
    let (info, _) = precheck(key, text)?;
    let nar = storage.get_nar_stream(&info.url).await?;
    admit(key, text, nar, universe).await
}

async fn judge(
    key: &str,
    info: &NarInfo,
    file_size: u64,
    file_sha: &[u8; 32],
    digest: &sui_compat::store_path_scan::NarDigest,
    universe: &dyn RefUniverse,
) -> Result<(), AdmissionError> {
    let nix32 = |d: &[u8]| sui_compat::store_path::nix_base32_encode(d);
    if info.file_size != file_size {
        return Err(Refusal::FileSizeMismatch { declared: info.file_size, actual: file_size }.into());
    }
    if nar_content_key(&info.url).is_some_and(|k| k != *file_sha) {
        return Err(Refusal::NarKeyMismatch { url: info.url.clone(), actual: nix32(file_sha) }.into());
    }
    if declared_sha256("FileHash", &info.file_hash)? != file_sha {
        return Err(Refusal::FileHashMismatch { declared: info.file_hash.clone(), actual: nix32(file_sha) }.into());
    }
    if info.nar_size != digest.size {
        return Err(Refusal::NarSizeMismatch { declared: info.nar_size, actual: digest.size }.into());
    }
    if declared_sha256("NarHash", &info.nar_hash)? != digest.sha256 {
        return Err(Refusal::NarHashMismatch { declared: info.nar_hash.clone(), actual: nix32(&digest.sha256) }.into());
    }
    if info.ca.is_some() {
        return Ok(());
    }
    let listed: BTreeSet<&str> = info.references.iter().filter_map(|r| hash_part(r)).collect();
    let mut omitted = Vec::new();
    for named in &digest.named {
        if listed.contains(named.as_str()) {
            continue;
        }
        if named == key || universe.is_real(named).await? {
            omitted.push(named.clone());
        }
    }
    if omitted.is_empty() {
        Ok(())
    } else {
        Err(Refusal::OmittedReferences { hashes: omitted }.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use crate::storage::StorageBackend as _;

    pub(crate) const SELF: &str = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
    const STD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
    const DOC: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2";

    fn nar_of(dir: &std::path::Path) -> Vec<u8> {
        let mut buf = Vec::new();
        sui_compat::nar::NarWriter::write_path(&mut buf, dir).unwrap();
        buf
    }

    /// The cid receipt in miniature: a toolchain that is symlinks into
    /// `rust-std`, plus a file that names a path which does not exist.
    fn toolchain() -> (tempfile::TempDir, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        std::os::unix::fs::symlink(format!("/nix/store/{STD}-rust-std/lib"), out.join("lib")).unwrap();
        std::fs::write(out.join("README"), format!("see /nix/store/{DOC}-a-doc-example")).unwrap();
        let nar = nar_of(&out);
        (dir, nar)
    }

    fn narinfo(nar: &[u8], file: &[u8], compression: &str, refs: &[&str]) -> String {
        let n = NixHash::of(HashAlgorithm::Sha256, nar).to_nix32();
        let f = NixHash::of(HashAlgorithm::Sha256, file).to_nix32();
        format!(
            "StorePath: /nix/store/{SELF}-rust-stable\nURL: nar/{f}.nar\nCompression: {compression}\n\
             FileHash: sha256:{f}\nFileSize: {}\nNarHash: sha256:{n}\nNarSize: {}\nReferences: {}\n",
            file.len(),
            nar.len(),
            refs.join(" "),
        )
    }

    fn universe() -> HashSet<String> {
        [STD.to_string(), SELF.to_string()].into_iter().collect()
    }

    #[tokio::test]
    async fn a_narinfo_that_omits_a_named_real_path_is_refused() {
        let (_d, nar) = toolchain();
        let text = narinfo(&nar, &nar, "none", &[]);
        let err = admit_bytes(SELF, &text, &nar, &universe()).await.unwrap_err();
        match err {
            AdmissionError::Refused(Refusal::OmittedReferences { hashes }) => {
                assert_eq!(hashes, vec![STD.to_string()], "only the real path, not the doc example");
            }
            other => panic!("expected OmittedReferences, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_complete_narinfo_is_admitted_under_every_codec() {
        let (_d, nar) = toolchain();
        let refs = [format!("{STD}-rust-std")];
        let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
        let mut enc = xz2::write::XzEncoder::new(Vec::new(), 1);
        enc.write_all(&nar).unwrap();
        let packed = enc.finish().unwrap();
        let zst = zstd::encode_all(&nar[..], 1).unwrap();
        for (name, file) in [("none", nar.clone()), ("xz", packed), ("zstd", zst)] {
            let text = narinfo(&nar, &file, name, &refs);
            let ok = admit_bytes(SELF, &text, &file, &universe()).await.unwrap();
            assert_eq!(ok.key(), SELF);
            assert_eq!(ok.info().references, vec![format!("{STD}-rust-std")]);
        }
    }

    #[tokio::test]
    async fn a_self_reference_must_be_listed_too() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("rpath"), format!("/nix/store/{SELF}-rust-stable/lib")).unwrap();
        let nar = nar_of(&out);
        let err = admit_bytes(SELF, &narinfo(&nar, &nar, "none", &[]), &nar, &universe()).await.unwrap_err();
        assert!(matches!(err, AdmissionError::Refused(Refusal::OmittedReferences { .. })), "{err:?}");
        let me = format!("{SELF}-rust-stable");
        admit_bytes(SELF, &narinfo(&nar, &nar, "none", &[&me]), &nar, &universe()).await.unwrap();
    }

    #[tokio::test]
    async fn every_hash_and_size_field_is_checked_against_the_bytes() {
        let (_d, nar) = toolchain();
        let refs = [format!("{STD}-rust-std")];
        let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
        let good = narinfo(&nar, &nar, "none", &refs);
        let other = nar_of(tempfile::tempdir().unwrap().path());
        let (mine, theirs) = (
            NixHash::of(HashAlgorithm::Sha256, &nar).to_nix32(),
            NixHash::of(HashAlgorithm::Sha256, &other).to_nix32(),
        );
        let cases: Vec<(String, &str)> = vec![
            (good.replace(&format!("FileSize: {}", nar.len()), "FileSize: 1"), "file-size-mismatch"),
            (good.replace(&format!("FileHash: sha256:{mine}"), &format!("FileHash: sha256:{theirs}")), "file-hash-mismatch"),
            (good.replace(&format!("URL: nar/{mine}"), &format!("URL: nar/{theirs}")), "nar-key-mismatch"),
            (good.replace(&format!("NarHash: sha256:{mine}"), &format!("NarHash: sha256:{theirs}")), "nar-hash-mismatch"),
            (good.replace(&format!("URL: nar/{mine}"), "URL: nar/old-store-hash-layout"), "ok"),
            (good.replace("Compression: none", "Compression: bzip2"), "unsupported-compression"),
            (good.replace(&format!("/nix/store/{SELF}"), &format!("/nix/store/{STD}")), "key-mismatch"),
            (good.replace("URL: nar/", "URL: ../"), "unaddressable-url"),
            ("not a narinfo".to_string(), "malformed"),
        ];
        for (text, want) in cases {
            let got = match admit_bytes(SELF, &text, &nar, &universe()).await {
                Ok(_) => "ok",
                Err(AdmissionError::Refused(r)) => r.kind(),
                Err(e) => panic!("store error {e}"),
            };
            assert_eq!(got, want, "for:\n{text}");
        }
    }

    #[tokio::test]
    async fn the_nar_hash_is_checked_after_decoding() {
        let (_d, nar) = toolchain();
        let refs = [format!("{STD}-rust-std")];
        let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
        let zst = zstd::encode_all(&nar[..], 1).unwrap();
        let good = narinfo(&nar, &zst, "zstd", &refs);
        let lie = good.replace(
            &NixHash::of(HashAlgorithm::Sha256, &nar).to_nix32(),
            &NixHash::of(HashAlgorithm::Sha256, b"other").to_nix32(),
        );
        let err = admit_bytes(SELF, &lie, &zst, &universe()).await.unwrap_err();
        assert!(matches!(err, AdmissionError::Refused(Refusal::NarHashMismatch { .. })), "{err:?}");
        let short = good.replace(&format!("NarSize: {}", nar.len()), "NarSize: 3");
        let err = admit_bytes(SELF, &short, &zst, &universe()).await.unwrap_err();
        assert!(matches!(err, AdmissionError::Refused(Refusal::NarSizeMismatch { .. })), "{err:?}");
        let truncated = &zst[..zst.len() / 2];
        let err = admit_bytes(SELF, &good, truncated, &universe()).await.unwrap_err();
        assert!(matches!(err, AdmissionError::Refused(Refusal::FileSizeMismatch { .. } | Refusal::NarCorrupt { .. })), "{err:?}");
    }

    #[tokio::test]
    async fn a_missing_nar_is_refused_and_a_content_address_skips_the_scan() {
        let (_d, nar) = toolchain();
        let text = narinfo(&nar, &nar, "none", &[]);
        let err = admit(SELF, &text, None, &universe()).await.unwrap_err();
        assert!(matches!(err, AdmissionError::Refused(Refusal::NarMissing { .. })), "{err:?}");
        let fod = format!("{text}CA: fixed:r:sha256:{}\n", NixHash::of(HashAlgorithm::Sha256, &nar).to_nix32());
        admit_bytes(SELF, &fod, &nar, &universe()).await.unwrap();
    }

    #[tokio::test]
    async fn the_universes_answer_from_their_own_contents() {
        let set: BTreeSet<String> = [STD.to_string()].into_iter().collect();
        assert!(set.is_real(STD).await.unwrap());
        assert!(!set.is_real(DOC).await.unwrap());
        let hset: HashSet<String> = [STD.to_string()].into_iter().collect();
        assert!(hset.is_real(STD).await.unwrap());
        assert!(!hset.is_real(DOC).await.unwrap());

        let dir = tempfile::tempdir().unwrap();
        let cache = crate::LocalStorage::new(dir.path());
        let e = fixture::entry(STD, "rust-std", b"std", &[]);
        cache.put_narinfo(&AdmittedNarInfo::unchecked_fixture(STD, &e.text)).await.unwrap();
        cache.put_kv_value(DOC, "an image tag").await.unwrap();
        let u = CacheUniverse(&cache);
        assert!(u.is_real(STD).await.unwrap(), "a servable narinfo makes a hash real");
        assert!(!u.is_real(DOC).await.unwrap(), "a non-narinfo value does not");
        assert!(!u.is_real(SELF).await.unwrap(), "an absent key does not");
    }

    #[test]
    fn a_content_key_is_the_nix32_file_hash_and_nothing_else_is() {
        let digest: [u8; 32] = sha2::Sha256::digest(b"bytes").into();
        for suffix in [".nar", ".nar.xz", ".nar.zst"] {
            let path = content_keyed_nar_path(&digest, suffix);
            assert_eq!(nar_content_key(&path), Some(digest), "{path}");
        }
        assert_eq!(nar_content_key(&format!("nar/{SELF}.nar.zst")), None, "store-hash layout");
        assert_eq!(nar_content_key("nar/x.nar"), None);
        assert_eq!(nar_content_key("other/aaaa.nar"), None);
        let path = content_keyed_nar_path(&digest, ".tar");
        assert_eq!(nar_content_key(&path), None, "not a NAR suffix");
        let mut bad = content_keyed_nar_path(&digest, ".nar");
        bad.replace_range(4..5, "e");
        assert_eq!(nar_content_key(&bad), None, "not nix32");
    }

    #[test]
    fn serve_time_admission_checks_key_shape_url_and_codec() {
        let e = fixture::entry(STD, "rust-std", b"std", &[]);
        assert!(is_servable_under(STD, &e.text));
        assert!(!is_servable_under(DOC, &e.text), "another key");
        assert!(!is_servable_under(STD, ""), "empty");
        assert!(!is_servable_under(STD, &e.text.replace("URL: nar/", "URL: ../")), "traversal URL");
        assert!(!is_servable_under(STD, &e.text.replace("Compression: none", "Compression: br")), "codec");
    }

    #[tokio::test]
    async fn resign_stored_rewrites_only_signatures() {
        let dir = tempfile::tempdir().unwrap();
        let cache = crate::LocalStorage::new(dir.path());
        assert!(!resign_stored(&cache, STD, |_| vec!["k:s".into()]).await.unwrap(), "absent entry");
        let e = fixture::entry(STD, "rust-std", b"std", &[]);
        cache.put_narinfo(&AdmittedNarInfo::unchecked_fixture(STD, &e.text)).await.unwrap();
        assert!(resign_stored(&cache, STD, |_| vec!["k:s".into()]).await.unwrap());
        let stored = NarInfo::parse(&cache.get_narinfo(STD).await.unwrap().unwrap()).unwrap();
        assert_eq!(stored.signatures, vec!["k:s".to_string()]);
        assert_eq!(NarInfo { signatures: vec![], ..stored }, NarInfo::parse(&e.text).unwrap());
        cache.put_kv_value(DOC, "not a narinfo").await.unwrap();
        assert!(resign_stored(&cache, DOC, |_| vec![]).await.is_err(), "unparseable entry is an error");
    }

    #[tokio::test]
    async fn a_raw_kv_write_cannot_store_a_narinfo() {
        let dir = tempfile::tempdir().unwrap();
        let cache = crate::LocalStorage::new(dir.path());
        let e = fixture::entry(STD, "rust-std", b"std", &[]);
        assert!(cache.put_kv_value(STD, &e.text).await.is_err());
        assert!(cache.put_kv_value(STD, "URL: nar/x.nar").await.is_err());
        assert!(cache.get_narinfo(STD).await.unwrap().is_none());
        cache.put_kv_value(STD, "registry.example/img:tag").await.unwrap();
        assert_eq!(cache.get_narinfo(STD).await.unwrap().as_deref(), Some("registry.example/img:tag"));
    }

    #[test]
    fn every_refusal_names_its_evidence() {
        let r = Refusal::OmittedReferences { hashes: vec![STD.to_string()] };
        assert!(r.to_string().contains(STD));
        assert_eq!(r.kind(), "omitted-references");
        let r = Refusal::NarSizeMismatch { declared: 7, actual: 9 };
        let s = r.to_string();
        assert!(s.contains('7') && s.contains('9'), "{s}");
        let e: StoreError = AdmissionError::Refused(r.clone()).into();
        assert!(matches!(e, StoreError::Refused(ref got) if *got == r));
        assert!(e.to_string().contains("nar-size-mismatch"));
    }

    #[tokio::test]
    async fn resigning_changes_only_signatures() {
        let (_d, nar) = toolchain();
        let refs = [format!("{STD}-rust-std")];
        let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
        let ok = admit_bytes(SELF, &narinfo(&nar, &nar, "none", &refs), &nar, &universe()).await.unwrap();
        let signed = ok.with_signatures(vec!["k:sig".into()]);
        assert_eq!(signed.info().signatures, vec!["k:sig".to_string()]);
        assert_eq!(NarInfo { signatures: vec![], ..signed.info().clone() }, *ok.info());
        assert!(signed.text().contains("Sig: k:sig"));
    }
}
