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
            Value::String(ns) => (ns.chars.to_string(), None),
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
        Ok(Value::Path(Box::new(SmolStr::from(path.to_string_lossy().as_ref()))))
    });

    // fetchTarball
    register_builtin(builtins, "fetchTarball", |args| {
        let (url, expected_sha256) = match &args[0] {
            Value::String(ns) => (ns.chars.to_string(), None),
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
        Ok(Value::Path(Box::new(SmolStr::from(extract_dir.to_string_lossy().as_ref()))))
    });
}

// ── Fetch implementation functions ────────────────────────────

/// Implement `builtins.fetchGit`. Accepts a string URL or attrset
/// `{ url; rev?; ref?; submodules?; }`. Shells out to `git` to clone
/// into a content-addressed temp directory and constructs the
/// CppNix-shaped result attrset.
pub(crate) fn fetch_git(arg: &Value) -> Result<Value, EvalError> {
    let (url, ref_opt, rev_opt, submodules) = match arg {
        Value::String(ns) => (ns.chars.to_string(), None, None, false),
        Value::Path(p) => (p.to_string(), None, None, false),
        Value::Attrs(a) => {
            let url = a
                .get("url")
                .ok_or_else(|| EvalError::AttrNotFound("url".into()))?
                .to_str()?;
            let r = a.get("ref").map(|v| v.to_str()).transpose()?;
            let rev = a.get("rev").map(|v| v.to_str()).transpose()?;
            let sub = a
                .get("submodules")
                .map(|v| v.as_bool().unwrap_or(false))
                .unwrap_or(false);
            (url, r, rev, sub)
        }
        _ => return Err(EvalError::TypeError("fetchGit: expected string or attrset".into())),
    };
    let key = format!("{url}\n{ref_opt:?}\n{rev_opt:?}\n{submodules}");
    use sha2::{Digest, Sha256};
    let cache_hash = format!("{:x}", Sha256::digest(key.as_bytes()));
    let target = std::env::temp_dir()
        .join("sui-fetchGit")
        .join(&cache_hash);
    let head_ref = ref_opt.as_deref().unwrap_or("HEAD");
    if !target.exists() {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| EvalError::IoError {
                context: format!("fetchGit: {}", target.display()),
                message: e.to_string(),
            })?;
        }
        let shallow = rev_opt.is_none();
        let branch = if ref_opt.is_some() && rev_opt.is_none() {
            Some(head_ref)
        } else {
            None
        };
        if let Err(e) = crate::git::clone(&url, &target, branch, shallow, submodules) {
            let _ = std::fs::remove_dir_all(&target);
            return Err(EvalError::IoError {
                context: format!("fetchGit: git clone {url}"),
                message: e,
            });
        }
        if let Some(rev) = &rev_opt {
            crate::git::checkout_rev(&target, rev).map_err(|e| EvalError::IoError {
                context: format!("fetchGit: git checkout {rev}"),
                message: e,
            })?;
        }
    }
    git_result_attrs(&target, submodules)
}

/// Read git metadata from the already-cloned target directory and
/// assemble the result attrset.
pub(crate) fn git_result_attrs(target: &std::path::Path, submodules: bool) -> Result<Value, EvalError> {
    let target_str = target.to_string_lossy().into_owned();
    let rev = crate::git::head_rev(target).unwrap_or_default();
    let short_rev = if rev.len() >= 7 { rev[..7].to_string() } else { rev.clone() };
    let last_modified: i64 = crate::git::head_timestamp(target).unwrap_or(0);
    let rev_count: i64 = crate::git::rev_count(target).unwrap_or(0);
    let last_modified_date = format_unix_yyyymmddhhmmss(last_modified);
    use sha2::{Digest, Sha256};
    let narhash_hex = format!("{:x}", Sha256::digest(rev.as_bytes()));

    let mut result = NixAttrs::new();
    result.insert("outPath".into(), Value::Path(Box::new(SmolStr::from(target_str.as_str()))));
    result.insert("rev".into(), Value::string(rev));
    result.insert("shortRev".into(), Value::string(short_rev));
    result.insert("revCount".into(), Value::Int(rev_count));
    result.insert("lastModified".into(), Value::Int(last_modified));
    result.insert("lastModifiedDate".into(), Value::string(last_modified_date));
    result.insert(
        "narHash".into(),
        Value::string(format!("sha256-{}", base64_encode(&hex_to_bytes(&narhash_hex)))),
    );
    result.insert("submodules".into(), Value::Bool(submodules));
    Ok(Value::Attrs(Rc::new(result)))
}

/// Implement `builtins.fetchMercurial`.
pub(crate) fn fetch_mercurial(arg: &Value) -> Result<Value, EvalError> {
    let (url, rev_opt) = match arg {
        Value::String(ns) => (ns.chars.to_string(), None),
        Value::Attrs(a) => {
            let url = a
                .get("url")
                .ok_or_else(|| EvalError::AttrNotFound("url".into()))?
                .to_str()?;
            let rev = a.get("rev").map(|v| v.to_str()).transpose()?;
            (url, rev)
        }
        _ => {
            return Err(EvalError::TypeError(
                "fetchMercurial: expected string or attrset".into(),
            ))
        }
    };
    use sha2::{Digest, Sha256};
    let key = format!("{url}\n{rev_opt:?}");
    let cache_hash = format!("{:x}", Sha256::digest(key.as_bytes()));
    let target = std::env::temp_dir()
        .join("sui-fetchMercurial")
        .join(&cache_hash);
    if !target.exists() {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| EvalError::IoError {
                context: format!("fetchMercurial: {}", target.display()),
                message: e.to_string(),
            })?;
        }
        let status = std::process::Command::new("hg")
            .args(["clone", &url, &target.to_string_lossy()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| EvalError::IoError {
                context: format!("fetchMercurial: spawn hg for {url}"),
                message: e.to_string(),
            })?;
        if !status.success() {
            let _ = std::fs::remove_dir_all(&target);
            return Err(EvalError::IoError {
                context: format!("fetchMercurial: hg clone {url}"),
                message: format!("hg clone exited with {status}"),
            });
        }
        if let Some(rev) = &rev_opt {
            let _ = std::process::Command::new("hg")
                .args(["-R", &target.to_string_lossy(), "update", rev])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    let mut result = NixAttrs::new();
    result.insert(
        "outPath".into(),
        Value::Path(Box::new(SmolStr::from(target.to_string_lossy().as_ref()))),
    );
    let rev = rev_opt.unwrap_or_else(|| "tip".into());
    result.insert("rev".into(), Value::string(rev.clone()));
    result.insert("revCount".into(), Value::Int(0));
    result.insert(
        "branch".into(),
        Value::string("default".to_string()),
    );
    Ok(Value::Attrs(Rc::new(result)))
}

/// Implement `builtins.fetchTree`. Dispatches on the `type` attr.
pub(crate) fn fetch_tree(arg: &Value) -> Result<Value, EvalError> {
    let attrs = match arg {
        Value::String(ns) => match parse_flake_ref(&ns.chars)? {
            Value::Attrs(a) => a,
            _ => unreachable!(),
        },
        Value::Attrs(a) => a.clone(),
        _ => {
            return Err(EvalError::TypeError(
                "fetchTree: expected string or attrset".into(),
            ))
        }
    };
    let ty = attrs
        .get("type")
        .ok_or_else(|| EvalError::AttrNotFound("type".into()))?
        .to_str()?;
    match ty.as_str() {
        "github" => {
            let owner = attrs
                .get("owner")
                .ok_or_else(|| EvalError::AttrNotFound("owner".into()))?
                .to_str()?;
            let repo = attrs
                .get("repo")
                .ok_or_else(|| EvalError::AttrNotFound("repo".into()))?
                .to_str()?;
            let reff = attrs
                .get("rev")
                .or_else(|| attrs.get("ref"))
                .map(|v| v.to_str())
                .transpose()?
                .unwrap_or_else(|| "HEAD".into());
            let url = format!("https://github.com/{owner}/{repo}.git");
            let mut g = NixAttrs::new();
            g.insert("url".into(), Value::string(url));
            g.insert("ref".into(), Value::string(reff));
            fetch_git(&Value::Attrs(Rc::new(g)))
        }
        "git" => {
            let mut g = NixAttrs::new();
            for (k, v) in attrs.iter() {
                if k != "type" {
                    g.insert(k.clone(), v.clone());
                }
            }
            fetch_git(&Value::Attrs(Rc::new(g)))
        }
        "tarball" => {
            let url_v = attrs
                .get("url")
                .ok_or_else(|| EvalError::AttrNotFound("url".into()))?
                .clone();
            let mut a = NixAttrs::new();
            a.insert("url".into(), url_v);
            let url = a
                .get("url")
                .ok_or_else(|| EvalError::AttrNotFound("url".into()))?
                .to_str()?;
            let bytes = fetch_url_bytes(&url)
                .map_err(|e| EvalError::TypeError(format!("fetchTree(tarball): {e}")))?;
            use sha2::{Digest, Sha256};
            let hash = format!("{:x}", Sha256::digest(&bytes));
            let base_dir = std::env::temp_dir().join("sui-fetchTree-tarball");
            let extract_dir = base_dir.join(&hash);
            if !extract_dir.exists() {
                std::fs::create_dir_all(&extract_dir).map_err(|e| EvalError::IoError {
                    context: format!("fetchTree(tarball): {}", extract_dir.display()),
                    message: e.to_string(),
                })?;
                let decoder = flate2::read::GzDecoder::new(&bytes[..]);
                let mut archive = tar::Archive::new(decoder);
                archive.unpack(&extract_dir).map_err(|e| EvalError::IoError {
                    context: format!("fetchTree(tarball): {}", extract_dir.display()),
                    message: e.to_string(),
                })?;
            }
            let mut result = NixAttrs::new();
            result.insert(
                "outPath".into(),
                Value::Path(Box::new(SmolStr::from(extract_dir.to_string_lossy().as_ref()))),
            );
            result.insert(
                "narHash".into(),
                Value::string(format!("sha256-{hash}")),
            );
            Ok(Value::Attrs(Rc::new(result)))
        }
        "path" => {
            let p = attrs
                .get("path")
                .ok_or_else(|| EvalError::AttrNotFound("path".into()))?
                .to_str()?;
            let mut result = NixAttrs::new();
            result.insert("outPath".into(), Value::Path(Box::new(SmolStr::from(p.as_str()))));
            Ok(Value::Attrs(Rc::new(result)))
        }
        other => Err(EvalError::NotImplemented(format!(
            "fetchTree: unsupported type '{other}'"
        ))),
    }
}

/// Fetch bytes from a URL. Supports `file://` scheme for local files and
/// delegates to `ureq` (synchronous, no tokio runtime) for HTTP(S).
pub(crate) fn fetch_url_bytes(url: &str) -> Result<Vec<u8>, String> {
    if let Some(path) = url.strip_prefix("file://") {
        std::fs::read(path).map_err(|e| format!("{e}"))
    } else {
        let resp = ureq::get(url).call().map_err(|e| format!("{e}"))?;
        resp.into_body()
            .read_to_vec()
            .map_err(|e| format!("{e}"))
    }
}

// ── Date/hash helpers ─────────────────────────────────────────

pub(crate) fn format_unix_yyyymmddhhmmss(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let secs_in_day = secs.rem_euclid(86400);
    let h = secs_in_day / 3600;
    let mi = (secs_in_day % 3600) / 60;
    let s = secs_in_day % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let mut y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as i64;
    let m = if mp < 10 { mp as i64 + 3 } else { mp as i64 - 9 };
    if m <= 2 {
        y += 1;
    }
    format!("{y:04}{m:02}{d:02}{h:02}{mi:02}{s:02}")
}

pub(crate) fn hex_to_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0))
        .collect()
}

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
