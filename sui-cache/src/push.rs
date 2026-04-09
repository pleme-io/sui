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

use crate::signing::CacheSigner;
use crate::storage::StorageBackend;
use crate::CacheError;

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

    // 3. Compress with xz.
    let compressed = compress_xz(&nar_data)?;
    let compressed_size = compressed.len() as u64;

    // 4. Hash compressed NAR.
    let file_hash = sha256_hex(&compressed);

    // 5. Build narinfo.
    let nar_url = format!("nar/{hash}.nar.xz");
    let narinfo = NarInfo {
        store_path: store_path.to_string(),
        url: nar_url.clone(),
        compression: "xz".to_string(),
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

/// Compress data with xz (level 6).
fn compress_xz(data: &[u8]) -> Result<Vec<u8>, CacheError> {
    let mut compressed = Vec::new();
    let mut encoder = xz2::write::XzEncoder::new(&mut compressed, 6);
    encoder.write_all(data).map_err(CacheError::Io)?;
    encoder.finish().map_err(CacheError::Io)?;
    Ok(compressed)
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
    use crate::signing::CacheSigner;
    use crate::storage::local::LocalStorage;

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
        assert_eq!(parsed.compression, "xz");
        assert_eq!(parsed.signatures.len(), 1);
        assert!(parsed.signatures[0].starts_with("test-cache:"));

        // Verify NAR blob was uploaded.
        let nar = storage.get_nar("nar/abc.nar.xz").await.unwrap().unwrap();
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

        push_path(
            &storage,
            &signer,
            path.to_str().unwrap(),
            "ttt",
            &[],
            None,
        )
        .await
        .unwrap();

        let narinfo_text = storage.get_narinfo("ttt").await.unwrap().unwrap();
        let parsed = NarInfo::parse(&narinfo_text).unwrap();

        // Verify the signature.
        let valid = crate::signing::verify_narinfo_signature(
            &parsed,
            &parsed.signatures[0],
            &pk_str,
        )
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
    fn compress_xz_produces_valid_output() {
        let data = b"hello world, this is test data for xz compression";
        let compressed = compress_xz(data).unwrap();
        // Decompress to verify.
        use std::io::Read;
        let mut decoder = xz2::read::XzDecoder::new(compressed.as_slice());
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        assert_eq!(decompressed, data);
    }
}
