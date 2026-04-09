//! Fetch builtins: fetchGit, fetchMercurial, fetchTree, fetchurl, fetchTarball.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    register_builtin(builtins, "fetchGit", |args| fetch_git(&args[0]));
    register_builtin(builtins, "fetchMercurial", |args| {
        fetch_mercurial(&args[0])
    });
    register_builtin(builtins, "fetchTree", |args| fetch_tree(&args[0]));

    // fetchurl
    register_builtin(builtins, "fetchurl", |args| {
        let (url, expected_sha256) = match &args[0] {
            Value::String(ns) => (ns.chars.clone(), None),
            Value::Attrs(a) => {
                let u = a
                    .get("url")
                    .ok_or_else(|| EvalError::AttrNotFound("url".into()))?
                    .to_str()?;
                let sha = a
                    .get("sha256")
                    .map(|v| v.to_str())
                    .transpose()?;
                (u, sha)
            }
            _ => {
                return Err(EvalError::TypeError(
                    "fetchurl: expected string or attrset".into(),
                ))
            }
        };
        let bytes = fetch_url_bytes(&url)
            .map_err(|e| EvalError::TypeError(format!("fetchurl: {e}")))?;
        use sha2::{Digest, Sha256};
        let hash = format!("{:x}", Sha256::digest(&bytes));
        if let Some(ref expected) = expected_sha256
            && *expected != hash {
                return Err(EvalError::TypeError(format!(
                    "fetchurl: sha256 mismatch: expected {expected}, got {hash}"
                )));
            }
        let dir = std::env::temp_dir().join("sui-fetchurl");
        std::fs::create_dir_all(&dir)
            .map_err(|e| EvalError::TypeError(format!("fetchurl: {e}")))?;
        let path = dir.join(&hash);
        std::fs::write(&path, &bytes)
            .map_err(|e| EvalError::TypeError(format!("fetchurl: {e}")))?;
        Ok(Value::Path(path.to_string_lossy().to_string()))
    });

    // fetchTarball
    register_builtin(builtins, "fetchTarball", |args| {
        let (url, expected_sha256) = match &args[0] {
            Value::String(ns) => (ns.chars.clone(), None),
            Value::Attrs(a) => {
                let u = a
                    .get("url")
                    .ok_or_else(|| EvalError::AttrNotFound("url".into()))?
                    .to_str()?;
                let sha = a
                    .get("sha256")
                    .map(|v| v.to_str())
                    .transpose()?;
                (u, sha)
            }
            _ => {
                return Err(EvalError::TypeError(
                    "fetchTarball: expected string or attrset".into(),
                ))
            }
        };
        let bytes = fetch_url_bytes(&url)
            .map_err(|e| EvalError::TypeError(format!("fetchTarball: {e}")))?;
        use sha2::{Digest, Sha256};
        let hash = format!("{:x}", Sha256::digest(&bytes));
        if let Some(ref expected) = expected_sha256
            && *expected != hash {
                return Err(EvalError::TypeError(format!(
                    "fetchTarball: sha256 mismatch: expected {expected}, got {hash}"
                )));
            }
        let base_dir = std::env::temp_dir().join("sui-fetchTarball");
        let extract_dir = base_dir.join(&hash);
        if !extract_dir.exists() {
            std::fs::create_dir_all(&extract_dir)
                .map_err(|e| EvalError::TypeError(format!("fetchTarball: {e}")))?;
            let decoder = flate2::read::GzDecoder::new(&bytes[..]);
            let mut archive = tar::Archive::new(decoder);
            archive
                .unpack(&extract_dir)
                .map_err(|e| EvalError::TypeError(format!("fetchTarball: {e}")))?;
        }
        Ok(Value::Path(extract_dir.to_string_lossy().to_string()))
    });
}
