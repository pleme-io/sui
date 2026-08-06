//! Push pipeline — build output to NAR to sign to upload.
//!
//! Takes a store path, dumps it as NAR, compresses with xz,
//! builds narinfo metadata, signs it, and uploads both to the
//! configured storage backend.

use std::io::Write;
use std::path::Path;

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
/// 3. Compress with xz
/// 4. Hash the compressed NAR (sha256)
/// 5. Build narinfo metadata
/// 6. Sign the narinfo
/// 7. Upload NAR blob and narinfo
///
/// The `store_path` should be an absolute path like `/nix/store/abc-hello-1.0`.
/// The `hash` is the 32-character store path hash (the `abc` part).
///
/// `references` are the runtime dependency store path basenames.
pub async fn push_path(
    storage: &dyn StorageBackend,
    signer: &CacheSigner,
    store_path: &str,
    hash: &str,
    references: &[String],
    deriver: Option<&str>,
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
    let codec = NarCodec::default();
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
    NarWriter::write_path(&mut buf, path).map_err(|e| {
        CacheError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("NAR dump failed: {e}"),
        ))
    })?;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NarCodec {
    /// zstd at [`ZSTD_LEVEL`], multithreaded. The default.
    #[default]
    Zstd,
    /// xz level 6 — what this cache used before 2026-08-05. Kept selectable
    /// rather than deleted (★★ MODULARIZE, DON'T DELETE): it is still the
    /// right choice for an origin that is bandwidth-bound rather than
    /// CPU-bound, and it is what every already-stored path is packed with.
    Xz,
}

/// The measured knee (see [`NarCodec`]): 19x faster than xz -6 for four
/// percentage points of ratio. Not a round number chosen for looks.
const ZSTD_LEVEL: i32 = 12;

impl NarCodec {
    /// The `Compression:` field value. **nix's wire vocabulary, not ours** —
    /// verified 2026-08-05 by having nix write a zstd cache itself
    /// (`nix copy --to 'file://…?compression=zstd'`) and reading back what it
    /// emitted.
    #[must_use]
    pub fn narinfo_name(self) -> &'static str {
        match self {
            Self::Zstd => "zstd",
            Self::Xz => "xz",
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
            Self::Zstd => ".nar.zst",
            Self::Xz => ".nar.xz",
        }
    }

    /// Compress a NAR under this codec.
    ///
    /// zstd runs multithreaded across the machine's cores; `workers(0)` asks
    /// the library for one worker per core. A failure to enable threading is
    /// deliberately NOT fatal — it costs speed, never correctness, and a cache
    /// push that refuses to run is worse than a slow one.
    pub fn compress(self, data: &[u8]) -> Result<Vec<u8>, CacheError> {
        match self {
            Self::Zstd => {
                let mut out = Vec::new();
                let mut enc = zstd::Encoder::new(&mut out, ZSTD_LEVEL).map_err(CacheError::Io)?;
                let _ = enc.multithread(
                    u32::try_from(std::thread::available_parallelism().map_or(1, usize::from))
                        .unwrap_or(1),
                );
                enc.write_all(data).map_err(CacheError::Io)?;
                enc.finish().map_err(CacheError::Io)?;
                Ok(out)
            }
            Self::Xz => {
                let mut out = Vec::new();
                let mut enc = xz2::write::XzEncoder::new(&mut out, 6);
                enc.write_all(data).map_err(CacheError::Io)?;
                enc.finish().map_err(CacheError::Io)?;
                Ok(out)
            }
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

        push_path(&storage, &signer, path.to_str().unwrap(), "ttt", &[], None)
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

    #[test]
    fn every_codec_round_trips() {
        use std::io::Read;
        let data = b"hello world, this is test data for NAR compression";

        let xz = NarCodec::Xz.compress(data).unwrap();
        let mut d = xz2::read::XzDecoder::new(xz.as_slice());
        let mut out = Vec::new();
        d.read_to_end(&mut out).unwrap();
        assert_eq!(out, data, "xz must round-trip");

        let z = NarCodec::Zstd.compress(data).unwrap();
        let out = zstd::decode_all(z.as_slice()).unwrap();
        assert_eq!(out, data, "zstd must round-trip");
    }

    // ── ★ THE INVARIANT THIS TYPE EXISTS FOR ────────────────────────────
    // The codec used to be stated three times in `push_path` — the compress
    // call, the `.nar.xz` URL literal, and `compression: "xz"`. A narinfo that
    // disagrees with its bytes is not a cosmetic defect: every client fails to
    // decompress, so the cache serves corruption while reporting success.
    // These pin that the three can only ever come from one value.

    #[test]
    fn the_suffix_and_the_narinfo_name_agree_for_every_codec() {
        for codec in [NarCodec::Zstd, NarCodec::Xz] {
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
        assert_eq!(NarCodec::Zstd.narinfo_name(), "zstd");
        assert_eq!(NarCodec::Zstd.url_suffix(), ".nar.zst");
        assert_eq!(NarCodec::Xz.narinfo_name(), "xz");
        assert_eq!(NarCodec::Xz.url_suffix(), ".nar.xz");
    }

    #[test]
    fn the_default_codec_is_the_fast_one() {
        // The whole point of the change. If someone flips the default back to
        // xz, they should have to edit this test and say why — a 2483-path
        // closure cost FOUR HOURS under xz -6 on rio.
        assert_eq!(NarCodec::default(), NarCodec::Zstd);
    }
}
