//! Push pipeline — build output to NAR to sign to upload.
//!
//! Takes a store path, dumps it as NAR, compresses it under the configured
//! [`NarCodec`], builds narinfo metadata, signs it, and uploads both to the
//! configured storage backend.
//!
//! The codec is **typed configuration**, not a compile-time constant: it is a
//! field of [`CacheConfig`](crate::CacheConfig), which is a
//! [`shikumi::TieredConfig`] (★★ CONFIGURATION MANAGEMENT). See [`NarCodec`]
//! for why zstd is the prescribed default and why the level rides *inside* the
//! codec rather than beside it.

use std::fmt;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sui_compat::nar::NarWriter;
use sui_compat::narinfo::NarInfo;

use crate::CacheError;
use crate::StorageBackend;
use crate::signing::CacheSigner;

/// Result of pushing a single store path.
#[derive(Debug, Clone)]
pub struct PushResult {
    /// The store path hash used as the narinfo key.
    pub hash: String,
    /// Size of the compressed NAR blob uploaded.
    pub compressed_size: u64,
    /// Size of the uncompressed NAR.
    pub nar_size: u64,
}

/// Push a store path to the binary cache.
///
/// 1. Dump the path as NAR
/// 2. Hash the uncompressed NAR (sha256)
/// 3. Compress under `codec`
/// 4. Hash the compressed NAR (sha256)
/// 5. Build narinfo metadata
/// 6. Sign the narinfo
/// 7. Upload NAR blob and narinfo
///
/// The `store_path` should be an absolute path like `/nix/store/abc-hello-1.0`.
/// The `hash` is the 32-character store path hash (the `abc` part).
///
/// `references` are the runtime dependency store path basenames.
///
/// `codec` is the packing posture — resolved from
/// [`CacheConfig::nar_codec`](crate::CacheConfig::nar_codec), never chosen
/// here. It is one value and it is *threaded*, not re-stated: the bytes, the
/// URL suffix and the `Compression:` field below all read the same parameter,
/// so a caller has no way to pick zstd bytes and an `xz` narinfo. See
/// [`NarCodec`].
///
/// # Errors
///
/// [`CacheError::PathNotFound`] if `store_path` does not exist,
/// [`CacheError::Io`] if the NAR dump or compression fails, or whatever the
/// backend returns from the two uploads.
pub async fn push_path(
    storage: &dyn StorageBackend,
    signer: &CacheSigner,
    store_path: &str,
    hash: &str,
    references: &[String],
    deriver: Option<&str>,
    codec: NarCodec,
) -> Result<PushResult, CacheError> {
    let path = Path::new(store_path);
    if !path.exists() {
        return Err(CacheError::PathNotFound(store_path.to_string()));
    }

    // 1. Dump to NAR.
    let nar_data = dump_path_to_nar(path)?;

    // 2. Hash uncompressed NAR.
    let nar_hash = sha256_hex(&nar_data);
    let nar_size = nar_data.len() as u64;

    // 3. Compress. ONE codec value drives the bytes, the suffix and the
    //    narinfo field below — see `NarCodec`.
    let compressed = codec.compress(&nar_data)?;
    let compressed_size = compressed.len() as u64;

    // 4. Hash compressed NAR.
    let file_hash = sha256_hex(&compressed);

    // 5. Build narinfo.
    let nar_url = format!("nar/{hash}{suffix}", suffix = codec.url_suffix());
    let narinfo = NarInfo {
        store_path: store_path.to_string(),
        url: nar_url.clone(),
        compression: codec.narinfo_name().to_string(),
        file_hash: format!("sha256:{file_hash}"),
        file_size: compressed_size,
        nar_hash: format!("sha256:{nar_hash}"),
        nar_size,
        references: references.to_vec(),
        deriver: deriver.map(String::from),
        signatures: vec![],
        ca: None,
    };

    // 6. Sign.
    let sig = signer.sign_narinfo(&narinfo);
    let narinfo = NarInfo {
        signatures: vec![sig],
        ..narinfo
    };

    // 7. Upload.
    storage.put_nar(&nar_url, &compressed).await?;
    storage.put_narinfo(hash, &narinfo.serialize()).await?;

    Ok(PushResult {
        hash: hash.to_string(),
        compressed_size,
        nar_size,
    })
}

/// Dump a filesystem path to NAR format in memory.
fn dump_path_to_nar(path: &Path) -> Result<Vec<u8>, CacheError> {
    let mut buf = Vec::new();
    NarWriter::write_path(&mut buf, path)
        .map_err(|e| CacheError::Io(std::io::Error::other(format!("NAR dump failed: {e}"))))?;
    Ok(buf)
}

/// How a NAR is packed for the cache.
///
/// ── ★ ONE VALUE — THE BYTES, THE SUFFIX AND THE NARINFO ALL DERIVE ──────
/// The codec used to be stated in THREE disconnected places in `push_path`:
/// the call to `compress_xz`, the literal `.nar.xz` in the URL, and
/// `compression: "xz".to_string()` in the narinfo. Three declarations of one
/// fact, free to disagree — and disagreement is not a cosmetic bug: a narinfo
/// that says `xz` over zstd bytes makes EVERY client fail to decompress, so
/// the cache would serve corruption while reporting success. That is the
/// failure mode this type removes, by leaving no way to state the codec twice.
///
/// ── WHY zstd IS THE DEFAULT — MEASURED, NOT ASSUMED ─────────────────────
/// Benchmarked on a real 48 MB NAR (git 2.51.2), 10 cores, 2026-08-05:
///
/// ```text
///   codec              ms     size   %orig
///   xz -6  (previous)  8368   8 MB    17%
///   xz -6 -T0          7615   8 MB    17%     <- multithreading xz buys 9%
///   zstd -19 -T0      11298   8 MB    17%     <- SLOWER than xz for the ratio
///   zstd -12 -T0        440  10 MB    21%     <- 19x faster than xz -6
///   zstd -9  -T0        243  10 MB    22%     <- 34x faster
/// ```
///
/// Two beliefs died there. "Just add `-T0` to xz" gains 9%, not the order of
/// magnitude it promises — liblzma's block splitting barely engages at this
/// size. And zstd is only faster at *lower* levels; at -19 it loses to xz on
/// both axes. The knee is -12: 19x the speed for four percentage points of
/// ratio.
///
/// That trade is obviously right HERE and the reason is architectural: this
/// cache is a LOCAL origin serving a handful of fleet nodes over tailscale.
/// Bandwidth is cheap; CPU-hours on the fleet's only x86_64-linux builder are
/// not. MEASURED cost of the old default on rio 2026-08-05: a 2483-path
/// closure spent FOUR HOURS in single-threaded xz, and because nix runs the
/// post-build hook synchronously it blocked every build on that node — which
/// is a different bug (fixed by detaching the hook) that this default made
/// unsurvivable.
///
/// A mixed cache is fine and needs no migration: each narinfo declares its own
/// codec, so paths already stored as `.nar.xz` keep resolving while new pushes
/// land as `.nar.zst`.
///
/// ── WHY THE LEVEL LIVES *INSIDE* THE VARIANT ────────────────────────────
/// The level used to be a free-standing `const ZSTD_LEVEL`. Lifting it to a
/// sibling config field (`{ codec, level }`) would have been the obvious move
/// and is wrong: `level = 12` means nothing when `codec = Xz`, and `level = 9`
/// means two completely different things across the two codecs (near-max for
/// xz, mid-range for zstd). A pair whose second component is only meaningful
/// for some values of the first is a variant payload, not a field — so the
/// codec choice and its one tuning knob travel as one value that cannot be
/// split, reordered, or half-applied.
///
/// The levels are [`ZstdLevel`] / [`XzLevel`], not bare integers: `xz2`'s
/// encoder **panics** on a preset above 9, so an out-of-range level in a config
/// file used to be a crash waiting on the first push. It is now rejected where
/// the value is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "codec", rename_all = "lowercase")]
pub enum NarCodec {
    /// zstd, multithreaded. The prescribed default at
    /// [`ZstdLevel::PRESCRIBED`].
    Zstd {
        /// The compression level, bounded to zstd's accepted band.
        #[serde(default)]
        level: ZstdLevel,
    },
    /// xz — what this cache used before 2026-08-05. Kept selectable rather
    /// than deleted (★★ MODULARIZE, DON'T DELETE): it is still the right
    /// choice for an origin that is bandwidth-bound rather than CPU-bound, and
    /// it is what every already-stored path is packed with.
    Xz {
        /// The preset, bounded to xz's accepted band.
        #[serde(default)]
        level: XzLevel,
    },
}

impl Default for NarCodec {
    /// The fast path is what you get without asking — see the benchmark in the
    /// [`NarCodec`] docs.
    fn default() -> Self {
        Self::Zstd {
            level: ZstdLevel::default(),
        }
    }
}

/// A compression level that fell outside its codec's accepted band.
///
/// Returned by [`ZstdLevel::new`] / [`XzLevel::new`] and, through
/// `#[serde(try_from)]`, by deserializing a config that names an impossible
/// level — so a bad level is a **config-parse rejection**, not a panic on the
/// first push. (Tier-honest: parse-time-rejected, one rung below
/// truly-unrepresentable — the inner field is private, so the only way to build
/// a level is through the checked constructor.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelOutOfRange {
    /// Which codec's band was violated (`"zstd"` / `"xz"`).
    pub codec: &'static str,
    /// The rejected value.
    pub got: i64,
    /// The inclusive lower bound.
    pub min: i64,
    /// The inclusive upper bound.
    pub max: i64,
}

impl fmt::Display for LevelOutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} level {} is outside the accepted range {}..={}",
            self.codec, self.got, self.min, self.max
        )
    }
}

impl std::error::Error for LevelOutOfRange {}

/// A zstd compression level known to be inside the accepted band.
///
/// The band is `1..=22`, not zstd's full `ZSTD_minCLevel()..=22`. The negative
/// "ultra-fast" levels are real but unmeasured here, and the benchmark that
/// picked 12 only covers the positive range — admitting a level we have never
/// timed would be a knob with no evidence behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "i32", into = "i32")]
pub struct ZstdLevel(i32);

impl ZstdLevel {
    /// Lowest accepted level.
    pub const MIN: i32 = 1;
    /// Highest accepted level (zstd's maximum).
    pub const MAX: i32 = 22;
    /// The measured knee (see [`NarCodec`]): 19x faster than xz -6 for four
    /// percentage points of ratio. Not a round number chosen for looks.
    pub const PRESCRIBED: i32 = 12;

    /// Build a level, rejecting anything outside `MIN..=MAX`.
    ///
    /// # Errors
    ///
    /// [`LevelOutOfRange`] if `level` is outside the accepted band.
    pub const fn new(level: i32) -> Result<Self, LevelOutOfRange> {
        if level < Self::MIN || level > Self::MAX {
            return Err(LevelOutOfRange {
                codec: "zstd",
                got: level as i64,
                min: Self::MIN as i64,
                max: Self::MAX as i64,
            });
        }
        Ok(Self(level))
    }

    /// The level as the integer zstd's encoder wants.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

impl Default for ZstdLevel {
    fn default() -> Self {
        Self(Self::PRESCRIBED)
    }
}

impl TryFrom<i32> for ZstdLevel {
    type Error = LevelOutOfRange;
    fn try_from(v: i32) -> Result<Self, Self::Error> {
        Self::new(v)
    }
}

impl From<ZstdLevel> for i32 {
    fn from(v: ZstdLevel) -> Self {
        v.0
    }
}

/// An xz preset known to be inside the accepted band (`0..=9`).
///
/// The bound is load-bearing rather than decorative: `xz2::write::XzEncoder`
/// unwraps `Stream::new_easy_encoder`, so a preset of 10 **panics** the pushing
/// process rather than returning an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct XzLevel(u32);

impl XzLevel {
    /// Lowest accepted preset.
    pub const MIN: u32 = 0;
    /// Highest accepted preset — above this `xz2` panics.
    pub const MAX: u32 = 9;
    /// What this cache used before 2026-08-05, and therefore what every
    /// already-stored `.nar.xz` is packed with.
    pub const PRESCRIBED: u32 = 6;

    /// Build a preset, rejecting anything outside `MIN..=MAX`.
    ///
    /// # Errors
    ///
    /// [`LevelOutOfRange`] if `level` is outside the accepted band.
    pub const fn new(level: u32) -> Result<Self, LevelOutOfRange> {
        if level > Self::MAX {
            return Err(LevelOutOfRange {
                codec: "xz",
                got: level as i64,
                min: Self::MIN as i64,
                max: Self::MAX as i64,
            });
        }
        Ok(Self(level))
    }

    /// The preset as the integer `xz2`'s encoder wants.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Default for XzLevel {
    fn default() -> Self {
        Self(Self::PRESCRIBED)
    }
}

impl TryFrom<u32> for XzLevel {
    type Error = LevelOutOfRange;
    fn try_from(v: u32) -> Result<Self, Self::Error> {
        Self::new(v)
    }
}

impl From<XzLevel> for u32 {
    fn from(v: XzLevel) -> Self {
        v.0
    }
}

impl NarCodec {
    /// The `Compression:` field value. **nix's wire vocabulary, not ours** —
    /// verified 2026-08-05 by having nix write a zstd cache itself
    /// (`nix copy --to 'file://…?compression=zstd'`) and reading back what it
    /// emitted.
    ///
    /// The level is deliberately absent from this value: it is an *encoder*
    /// setting, and a decompressor reads it out of the frame header. A codec
    /// reconfigured from level 12 to level 3 still publishes `zstd`, and every
    /// client still reads it.
    #[must_use]
    pub fn narinfo_name(self) -> &'static str {
        match self {
            Self::Zstd { .. } => "zstd",
            Self::Xz { .. } => "xz",
        }
    }

    /// The NAR URL suffix.
    ///
    /// `.nar.zst`, NOT `.nar.zstd` — taken from nix's own output in the same
    /// experiment above. Guessing here would have produced a cache whose URLs
    /// no client resolves, and nothing in our own types would have objected.
    #[must_use]
    pub fn url_suffix(self) -> &'static str {
        match self {
            Self::Zstd { .. } => ".nar.zst",
            Self::Xz { .. } => ".nar.xz",
        }
    }

    /// Compress a NAR under this codec.
    ///
    /// zstd runs multithreaded across the machine's cores; `workers(0)` asks
    /// the library for one worker per core. A failure to enable threading is
    /// deliberately NOT fatal — it costs speed, never correctness, and a cache
    /// push that refuses to run is worse than a slow one.
    ///
    /// # Errors
    ///
    /// [`CacheError::Io`] if the encoder cannot be built or the write fails.
    /// The level cannot be the cause: it is bounded at construction.
    pub fn compress(self, data: &[u8]) -> Result<Vec<u8>, CacheError> {
        match self {
            Self::Zstd { level } => {
                let mut out = Vec::new();
                let mut enc = zstd::Encoder::new(&mut out, level.get()).map_err(CacheError::Io)?;
                let _ = enc.multithread(
                    u32::try_from(std::thread::available_parallelism().map_or(1, usize::from))
                        .unwrap_or(1),
                );
                enc.write_all(data).map_err(CacheError::Io)?;
                enc.finish().map_err(CacheError::Io)?;
                Ok(out)
            }
            Self::Xz { level } => {
                let mut out = Vec::new();
                // `level.get()` is bounded to 0..=9 by construction — the
                // unwrap inside `XzEncoder::new` cannot be reached.
                let mut enc = xz2::write::XzEncoder::new(&mut out, level.get());
                enc.write_all(data).map_err(CacheError::Io)?;
                enc.finish().map_err(CacheError::Io)?;
                Ok(out)
            }
        }
    }

    /// Decompress a NAR packed under this codec.
    ///
    /// The inverse of [`compress`](Self::compress), on the same value — so a
    /// test (or a future serve-side verifier) can prove that what a narinfo
    /// *declares* actually decodes the bytes it points at, rather than
    /// asserting two strings match.
    ///
    /// # Errors
    ///
    /// [`CacheError::Io`] if the data is not a valid frame for this codec.
    pub fn decompress(self, data: &[u8]) -> Result<Vec<u8>, CacheError> {
        use std::io::Read;
        let mut out = Vec::new();
        match self {
            Self::Zstd { .. } => {
                zstd::Decoder::new(data)
                    .map_err(CacheError::Io)?
                    .read_to_end(&mut out)
                    .map_err(CacheError::Io)?;
            }
            Self::Xz { .. } => {
                xz2::read::XzDecoder::new(data)
                    .read_to_end(&mut out)
                    .map_err(CacheError::Io)?;
            }
        }
        Ok(out)
    }

    /// Resolve a codec from the `Compression:` field of a narinfo — the
    /// *reader's* half of the one-value invariant.
    ///
    /// Level is irrelevant on the decode side (it lives in the frame header),
    /// so the returned value carries the prescribed level as a placeholder and
    /// is only ever used for its [`decompress`](Self::decompress) /
    /// [`url_suffix`](Self::url_suffix) projections.
    #[must_use]
    pub fn from_narinfo_name(name: &str) -> Option<Self> {
        match name {
            "zstd" => Some(Self::Zstd {
                level: ZstdLevel::default(),
            }),
            "xz" => Some(Self::Xz {
                level: XzLevel::default(),
            }),
            _ => None,
        }
    }
}

/// Compute SHA-256 hash and return lowercase hex.
fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in digest.as_slice() {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalStorage;
    use crate::signing::CacheSigner;

    #[tokio::test]
    async fn push_single_file() {
        let cache_dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(cache_dir.path());
        let signer = CacheSigner::generate("test-cache".to_string());

        // Create a store path to push.
        let store_dir = tempfile::tempdir().unwrap();
        let fake_store = store_dir.path().join("nix/store/abc-hello-1.0");
        std::fs::create_dir_all(&fake_store).unwrap();
        std::fs::write(fake_store.join("hello.txt"), b"Hello world!").unwrap();

        let result = push_path(
            &storage,
            &signer,
            fake_store.to_str().unwrap(),
            "abc",
            &[],
            None,
            NarCodec::default(),
        )
        .await
        .unwrap();

        assert_eq!(result.hash, "abc");
        assert!(result.nar_size > 0);
        assert!(result.compressed_size > 0);

        // Verify narinfo was uploaded.
        let narinfo = storage.get_narinfo("abc").await.unwrap().unwrap();
        let parsed = NarInfo::parse(&narinfo).unwrap();
        // Derived from NarCodec::default(), never restated — asserting a
        // literal here is what let the bytes and the narinfo drift apart in
        // the first place.
        assert_eq!(parsed.compression, NarCodec::default().narinfo_name());
        assert_eq!(parsed.signatures.len(), 1);
        assert!(parsed.signatures[0].starts_with("test-cache:"));

        // Verify NAR blob was uploaded.
        let nar_key = format!("nar/abc{}", NarCodec::default().url_suffix());
        let nar = storage.get_nar(&nar_key).await.unwrap().unwrap();
        assert!(!nar.is_empty());
    }

    #[tokio::test]
    async fn push_nonexistent_path_errors() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let signer = CacheSigner::generate("k".to_string());

        let result = push_path(
            &storage,
            &signer,
            "/nix/store/does-not-exist-12345",
            "nope",
            &[],
            None,
            NarCodec::default(),
        )
        .await;

        assert!(result.is_err());
        assert!(matches!(result, Err(CacheError::PathNotFound(_))));
    }

    #[tokio::test]
    async fn push_with_references() {
        let cache_dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(cache_dir.path());
        let signer = CacheSigner::generate("k".to_string());

        let store_dir = tempfile::tempdir().unwrap();
        let path = store_dir.path().join("pkg");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("file"), b"data").unwrap();

        let refs = vec!["dep1-glibc".to_string(), "dep2-gcc".to_string()];
        let result = push_path(
            &storage,
            &signer,
            path.to_str().unwrap(),
            "xyz",
            &refs,
            Some("builder.drv"),
            NarCodec::default(),
        )
        .await
        .unwrap();

        assert_eq!(result.hash, "xyz");

        let narinfo = storage.get_narinfo("xyz").await.unwrap().unwrap();
        let parsed = NarInfo::parse(&narinfo).unwrap();
        assert_eq!(parsed.references, refs);
        assert_eq!(parsed.deriver, Some("builder.drv".to_string()));
    }

    #[tokio::test]
    async fn pushed_narinfo_is_valid_and_verifiable() {
        let cache_dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(cache_dir.path());
        let signer = CacheSigner::generate("verify-key".to_string());
        let pk_str = signer.public_key_string();

        let store_dir = tempfile::tempdir().unwrap();
        let path = store_dir.path().join("test-pkg");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("data"), b"test content").unwrap();

        push_path(
            &storage,
            &signer,
            path.to_str().unwrap(),
            "ttt",
            &[],
            None,
            NarCodec::default(),
        )
        .await
        .unwrap();

        let narinfo_text = storage.get_narinfo("ttt").await.unwrap().unwrap();
        let parsed = NarInfo::parse(&narinfo_text).unwrap();

        // Verify the signature.
        let valid =
            crate::signing::verify_narinfo_signature(&parsed, &parsed.signatures[0], &pk_str)
                .unwrap();
        assert!(valid);
    }

    #[test]
    fn sha256_hex_produces_correct_output() {
        // SHA-256 of empty string is well-known.
        let hash = sha256_hex(b"");
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// Every codec the config surface can name — the closed set the
    /// invariant tests below sweep. A new variant that is not added here
    /// leaves the exhaustive `match` in [`codecs`] failing to compile, so the
    /// sweep cannot silently stop covering it.
    fn codecs() -> Vec<NarCodec> {
        // Exhaustive by construction: this match forces a compile error when a
        // variant is added, which is the point of writing it as a match over a
        // dummy rather than as a literal list.
        let all = [
            NarCodec::Zstd {
                level: ZstdLevel::default(),
            },
            NarCodec::Xz {
                level: XzLevel::default(),
            },
        ];
        for c in all {
            match c {
                NarCodec::Zstd { .. } | NarCodec::Xz { .. } => {}
            }
        }
        all.to_vec()
    }

    #[test]
    fn every_codec_round_trips() {
        use std::io::Read;
        let data = b"hello world, this is test data for NAR compression";

        let xz = NarCodec::Xz {
            level: XzLevel::default(),
        }
        .compress(data)
        .unwrap();
        let mut d = xz2::read::XzDecoder::new(xz.as_slice());
        let mut out = Vec::new();
        d.read_to_end(&mut out).unwrap();
        assert_eq!(out, data, "xz must round-trip");

        let z = NarCodec::Zstd {
            level: ZstdLevel::default(),
        }
        .compress(data)
        .unwrap();
        let out = zstd::decode_all(z.as_slice()).unwrap();
        assert_eq!(out, data, "zstd must round-trip");
    }

    #[test]
    fn every_configurable_level_round_trips_under_its_own_codec() {
        // The level is now operator-supplied, so the round-trip has to hold
        // across the band, not only at the prescribed knee.
        let data = b"hello world, this is test data for NAR compression".repeat(64);
        for level in [ZstdLevel::MIN, ZstdLevel::PRESCRIBED, 19, ZstdLevel::MAX] {
            let codec = NarCodec::Zstd {
                level: ZstdLevel::new(level).unwrap(),
            };
            assert_eq!(
                codec.decompress(&codec.compress(&data).unwrap()).unwrap(),
                data
            );
        }
        for level in [XzLevel::MIN, XzLevel::PRESCRIBED, XzLevel::MAX] {
            let codec = NarCodec::Xz {
                level: XzLevel::new(level).unwrap(),
            };
            assert_eq!(
                codec.decompress(&codec.compress(&data).unwrap()).unwrap(),
                data
            );
        }
    }

    #[test]
    fn an_out_of_band_level_is_rejected_where_it_is_built() {
        // xz2's encoder PANICS above preset 9 — the bound is what stops a
        // config typo from taking the pushing process down mid-closure.
        assert!(XzLevel::new(10).is_err(), "xz preset 10 panics xz2");
        assert!(XzLevel::new(u32::MAX).is_err());
        assert!(ZstdLevel::new(0).is_err());
        assert!(ZstdLevel::new(23).is_err());
        assert!(ZstdLevel::new(-5).is_err(), "unmeasured ultra-fast band");

        // …and the rejection reaches config parsing, so a bad YAML/JSON level
        // is a startup error rather than a first-push surprise.
        let bad = r#"{ "codec": "xz", "level": 10 }"#;
        assert!(
            serde_json::from_str::<NarCodec>(bad).is_err(),
            "a config naming an impossible level must fail to parse"
        );
        let good = r#"{ "codec": "xz", "level": 9 }"#;
        assert_eq!(
            serde_json::from_str::<NarCodec>(good).unwrap(),
            NarCodec::Xz {
                level: XzLevel::new(9).unwrap()
            }
        );
    }

    #[test]
    fn a_codec_without_a_level_takes_its_own_prescribed_one() {
        // The level rides inside the variant, so omitting it in config picks
        // the level that belongs to THAT codec — 12 for zstd, 6 for xz. A
        // shared `level` field could not have done this.
        assert_eq!(
            serde_json::from_str::<NarCodec>(r#"{ "codec": "zstd" }"#).unwrap(),
            NarCodec::Zstd {
                level: ZstdLevel::new(12).unwrap()
            }
        );
        assert_eq!(
            serde_json::from_str::<NarCodec>(r#"{ "codec": "xz" }"#).unwrap(),
            NarCodec::Xz {
                level: XzLevel::new(6).unwrap()
            }
        );
    }

    // ── ★ THE INVARIANT THIS TYPE EXISTS FOR ────────────────────────────
    // The codec used to be stated three times in `push_path` — the compress
    // call, the `.nar.xz` URL literal, and `compression: "xz"`. A narinfo that
    // disagrees with its bytes is not a cosmetic defect: every client fails to
    // decompress, so the cache serves corruption while reporting success.
    // These pin that the three can only ever come from one value.

    #[test]
    fn the_suffix_and_the_narinfo_name_agree_for_every_codec() {
        for codec in codecs() {
            let suffix = codec.url_suffix();
            let name = codec.narinfo_name();
            // `.nar.zst` carries `zstd`; `.nar.xz` carries `xz`. The suffix is
            // nix's spelling, not ours — hence the explicit pairing rather
            // than a string-derived assertion.
            let expected_suffix = match name {
                "zstd" => ".nar.zst",
                "xz" => ".nar.xz",
                other => panic!("unknown codec name {other} — add its suffix pairing"),
            };
            assert_eq!(
                suffix, expected_suffix,
                "codec {codec:?} would publish a URL its own Compression field \
                 does not describe; every client would fail to decompress"
            );
        }
    }

    #[test]
    fn the_narinfo_names_are_nix_wire_vocabulary() {
        // Verified 2026-08-05 against nix itself: `nix copy --to
        // 'file://…?compression=zstd'` emits `Compression: zstd` and
        // `URL: nar/….nar.zst`. These are nix's spellings, not ours, so they
        // are pinned rather than derived — `.nar.zstd` would have been the
        // natural guess and is WRONG.
        //
        // The level is varied deliberately: the wire vocabulary is a property
        // of the CODEC, never of its tuning, so a re-levelled codec must still
        // publish the same two strings.
        for level in [ZstdLevel::MIN, ZstdLevel::PRESCRIBED, ZstdLevel::MAX] {
            let c = NarCodec::Zstd {
                level: ZstdLevel::new(level).unwrap(),
            };
            assert_eq!(c.narinfo_name(), "zstd");
            assert_eq!(c.url_suffix(), ".nar.zst");
        }
        for level in [XzLevel::MIN, XzLevel::PRESCRIBED, XzLevel::MAX] {
            let c = NarCodec::Xz {
                level: XzLevel::new(level).unwrap(),
            };
            assert_eq!(c.narinfo_name(), "xz");
            assert_eq!(c.url_suffix(), ".nar.xz");
        }
    }

    #[test]
    fn the_default_codec_is_the_fast_one() {
        // The whole point of the change. If someone flips the default back to
        // xz, they should have to edit this test and say why — a 2483-path
        // closure cost FOUR HOURS under xz -6 on rio.
        //
        // Now that the codec is CONFIGURABLE the guarantee is bigger than the
        // `Default` impl: the prescribed shikumi tier — what an operator who
        // sets nothing actually gets — must be the fast one too. A default
        // that is fast while the prescribed tier is slow would satisfy the old
        // assertion and still hand every unconfigured origin xz.
        assert_eq!(
            NarCodec::default(),
            NarCodec::Zstd {
                level: ZstdLevel::new(ZstdLevel::PRESCRIBED).unwrap()
            }
        );
        assert_eq!(
            crate::CacheConfig::default().nar_codec,
            NarCodec::default(),
            "CacheConfig::default() must not describe a different cache"
        );
        assert_eq!(
            <crate::CacheConfig as shikumi::TieredConfig>::prescribed_default().nar_codec,
            NarCodec::default(),
            "an operator who configures nothing must get the measured fast path"
        );
    }

    // ── ★ THE DRIFT CLASS, UNDER CONFIGURATION ──────────────────────────
    // Making the codec configurable introduces the risk the type was built to
    // remove: a NON-default codec is now reachable in production, so it must
    // agree with itself just as tightly as the default does.

    #[tokio::test]
    async fn a_configured_non_default_codec_still_agrees_end_to_end() {
        // Not a string comparison: for EVERY codec the config can name, push a
        // real path, then decode the stored blob using ONLY what the narinfo
        // declares — the codec resolved from its `Compression:` field, at the
        // URL its `URL:` field names — and check the bytes hash to the
        // `NarHash:` it advertises. That is exactly what a nix client does, so
        // a narinfo that disagrees with its bytes fails here the way it would
        // fail in the field, rather than passing a lint.
        for codec in codecs() {
            assert_ne!(
                codec.narinfo_name(),
                "",
                "every codec must name itself on the wire"
            );
            let cache_dir = tempfile::tempdir().unwrap();
            let storage = LocalStorage::new(cache_dir.path());
            let signer = CacheSigner::generate("cfg-key".to_string());

            let store_dir = tempfile::tempdir().unwrap();
            let path = store_dir.path().join("cfg-pkg");
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("payload"),
                b"configured-codec payload".repeat(512),
            )
            .unwrap();

            push_path(
                &storage,
                &signer,
                path.to_str().unwrap(),
                "cfg",
                &[],
                None,
                codec,
            )
            .await
            .unwrap();

            let parsed =
                NarInfo::parse(&storage.get_narinfo("cfg").await.unwrap().unwrap()).unwrap();

            // (a) The declared codec is a codec we can actually resolve.
            let declared = NarCodec::from_narinfo_name(&parsed.compression)
                .unwrap_or_else(|| panic!("unresolvable Compression: {}", parsed.compression));

            // (b) The URL the narinfo publishes carries that codec's suffix.
            assert!(
                parsed.url.ends_with(declared.url_suffix()),
                "narinfo for {codec:?} publishes URL {} under Compression {} — \
                 the suffix and the field disagree",
                parsed.url,
                parsed.compression
            );

            // (c) The blob really is at that URL…
            let blob = storage
                .get_nar(&parsed.url)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("no NAR stored at the advertised URL {}", parsed.url));

            // (d) …and it decodes under the DECLARED codec, to bytes matching
            //     the declared NarHash. This is the assertion that would have
            //     caught zstd bytes wearing an `xz` label.
            let plain = declared.decompress(&blob).unwrap_or_else(|e| {
                panic!(
                    "narinfo declares {} but the bytes do not decode as it: {e}",
                    parsed.compression
                )
            });
            assert_eq!(
                parsed.nar_hash,
                format!("sha256:{}", sha256_hex(&plain)),
                "decoded bytes do not match the NarHash the narinfo advertises"
            );
            assert_eq!(parsed.nar_size, plain.len() as u64);
            assert_eq!(
                parsed.file_hash,
                format!("sha256:{}", sha256_hex(&blob)),
                "FileHash does not describe the stored blob"
            );
        }
    }

    #[tokio::test]
    async fn the_codec_a_config_names_is_the_codec_a_push_uses() {
        // The whole wire, from a YAML file on disk to the bytes in the cache,
        // through shikumi's REAL loader (`ConfigTier::Custom`) rather than a
        // hand-rolled parse — a knob that resolves but never reaches the
        // compressor is decorative, and the operator's belief about their
        // cache would be wrong.
        use shikumi::{ConfigTier, TieredConfig};

        let cfg_dir = tempfile::tempdir().unwrap();
        let cfg_path = cfg_dir.path().join("cache.yaml");
        std::fs::write(&cfg_path, "nar_codec:\n  codec: xz\n  level: 1\n").unwrap();

        let configured = crate::CacheConfig::resolve_tier(ConfigTier::Custom(cfg_path)).nar_codec;
        assert_eq!(
            configured,
            NarCodec::Xz {
                level: XzLevel::new(1).unwrap()
            },
            "the YAML overlay did not reach the codec field"
        );

        let cache_dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(cache_dir.path());
        let signer = CacheSigner::generate("cfg-key".to_string());
        let store_dir = tempfile::tempdir().unwrap();
        let path = store_dir.path().join("pkg");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("f"), b"data").unwrap();

        push_path(
            &storage,
            &signer,
            path.to_str().unwrap(),
            "cfgd",
            &[],
            None,
            configured,
        )
        .await
        .unwrap();

        let parsed = NarInfo::parse(&storage.get_narinfo("cfgd").await.unwrap().unwrap()).unwrap();
        assert_eq!(parsed.compression, "xz");
        assert_eq!(parsed.url, "nar/cfgd.nar.xz");
        // And it is NOT the default — otherwise this test would pass while the
        // configuration was being ignored entirely.
        assert_ne!(configured, NarCodec::default());
        assert_ne!(parsed.compression, NarCodec::default().narinfo_name());
    }
}
