//! Flake reference parsing: `parseFlakeRef` and `flakeRefToString`.
//!
//! Pure functions — no fetching, no registry lookup, no filesystem checks.

use crate::value::*;

/// Parse a flake reference string into the canonical attrset CppNix
/// returns from `builtins.parseFlakeRef`. Pure — no fetching, no
/// registry lookup, no filesystem checks. Returns an `EvalError`
/// only when the reference is structurally invalid.
pub(crate) fn parse_flake_ref(s: &str) -> Result<Value, EvalError> {
    // Helper: split "<base>?<query>" into (base, optional query map).
    fn split_query(s: &str) -> (&str, Vec<(String, String)>) {
        match s.split_once('?') {
            None => (s, Vec::new()),
            Some((base, q)) => {
                let params: Vec<(String, String)> = q
                    .split('&')
                    .filter(|p| !p.is_empty())
                    .map(|p| match p.split_once('=') {
                        Some((k, v)) => (k.to_string(), percent_decode(v)),
                        None => (p.to_string(), String::new()),
                    })
                    .collect();
                (base, params)
            }
        }
    }
    fn percent_decode(s: &str) -> String {
        // CppNix accepts %xx in query values; tolerate raw bytes too.
        let mut out = String::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len()
                && let Ok(b) = u8::from_str_radix(
                    std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00"),
                    16,
                ) {
                    out.push(b as char);
                    i += 3;
                    continue;
                }
            out.push(bytes[i] as char);
            i += 1;
        }
        out
    }

    let mut attrs = NixAttrs::new();

    // ── github / gitlab / sourcehut shorthand ────────────
    for (scheme, ty) in &[
        ("github:", "github"),
        ("gitlab:", "gitlab"),
        ("sourcehut:", "sourcehut"),
    ] {
        if let Some(rest) = s.strip_prefix(*scheme) {
            let (base, params) = split_query(rest);
            let parts: Vec<&str> = base.splitn(3, '/').collect();
            if parts.len() < 2 {
                return Err(EvalError::TypeError(format!(
                    "parseFlakeRef: '{s}' missing owner/repo"
                )));
            }
            attrs.insert("type".into(), Value::string(*ty));
            attrs.insert("owner".into(), Value::string(parts[0].to_string()));
            attrs.insert("repo".into(), Value::string(parts[1].to_string()));
            if let Some(reff) = parts.get(2)
                && !reff.is_empty() {
                    // Could be a ref or a 40-char hex sha (rev). CppNix
                    // returns it under "ref" either way for shorthand.
                    attrs.insert("ref".into(), Value::string((*reff).to_string()));
                }
            for (k, v) in params {
                attrs.insert(k, Value::string(v));
            }
            return Ok(Value::Attrs(Box::new(attrs)));
        }
    }

    // ── git+<scheme> ─────────────────────────────────────
    if let Some(rest) = s.strip_prefix("git+") {
        let (base, params) = split_query(rest);
        attrs.insert("type".into(), Value::string("git"));
        attrs.insert("url".into(), Value::string(base.to_string()));
        for (k, v) in params {
            attrs.insert(k, Value::string(v));
        }
        return Ok(Value::Attrs(Box::new(attrs)));
    }

    // ── tarball+<scheme> ─────────────────────────────────
    if let Some(rest) = s.strip_prefix("tarball+") {
        let (base, params) = split_query(rest);
        attrs.insert("type".into(), Value::string("tarball"));
        attrs.insert("url".into(), Value::string(base.to_string()));
        for (k, v) in params {
            attrs.insert(k, Value::string(v));
        }
        return Ok(Value::Attrs(Box::new(attrs)));
    }

    // ── path:<path> or absolute path ─────────────────────
    if let Some(p) = s.strip_prefix("path:") {
        attrs.insert("type".into(), Value::string("path"));
        attrs.insert("path".into(), Value::string(p.to_string()));
        return Ok(Value::Attrs(Box::new(attrs)));
    }
    if s.starts_with('/') {
        attrs.insert("type".into(), Value::string("path"));
        attrs.insert("path".into(), Value::string(s.to_string()));
        return Ok(Value::Attrs(Box::new(attrs)));
    }

    Err(EvalError::TypeError(format!(
        "parseFlakeRef: '{s}' is not a recognised flake reference"
    )))
}

/// Inverse of [`parse_flake_ref`] — render a flake-ref attrset back
/// to its canonical string form. Mirrors CppNix `flakeRefToString`,
/// including the ordering quirks (`type` first, query params sorted
/// alphabetically, `dir` always last for github-style refs etc.).
pub(crate) fn flake_ref_to_string(attrs: &NixAttrs) -> Result<Value, EvalError> {
    let ty = attrs
        .get("type")
        .ok_or_else(|| EvalError::AttrNotFound("type".into()))?
        .to_str()?;

    // Helper: collect all attrs other than the structural ones into
    // a sorted query string. CppNix sorts query params alphabetically
    // before serialising.
    fn query_string(attrs: &NixAttrs, exclude: &[&str]) -> Result<String, EvalError> {
        let mut params: Vec<(String, String)> = Vec::new();
        for (k, v) in attrs.iter() {
            if exclude.contains(&k.as_str()) {
                continue;
            }
            params.push((k.clone(), v.to_str()?));
        }
        params.sort_by(|a, b| a.0.cmp(&b.0));
        if params.is_empty() {
            return Ok(String::new());
        }
        let parts: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        Ok(format!("?{}", parts.join("&")))
    }

    match ty.as_str() {
        "github" | "gitlab" | "sourcehut" => {
            let owner = attrs
                .get("owner")
                .ok_or_else(|| EvalError::AttrNotFound("owner".into()))?
                .to_str()?;
            let repo = attrs
                .get("repo")
                .ok_or_else(|| EvalError::AttrNotFound("repo".into()))?
                .to_str()?;
            let mut out = format!("{ty}:{owner}/{repo}");
            // CppNix prefers rev over ref in the path component.
            if let Some(rev) = attrs.get("rev") {
                out.push('/');
                out.push_str(&rev.to_str()?);
            } else if let Some(reff) = attrs.get("ref") {
                out.push('/');
                out.push_str(&reff.to_str()?);
            }
            out.push_str(&query_string(
                attrs,
                &["type", "owner", "repo", "ref", "rev"],
            )?);
            Ok(Value::string(out))
        }
        "git" => {
            let url = attrs
                .get("url")
                .ok_or_else(|| EvalError::AttrNotFound("url".into()))?
                .to_str()?;
            let qs = query_string(attrs, &["type", "url"])?;
            Ok(Value::string(format!("git+{url}{qs}")))
        }
        "tarball" => {
            let url = attrs
                .get("url")
                .ok_or_else(|| EvalError::AttrNotFound("url".into()))?
                .to_str()?;
            let qs = query_string(attrs, &["type", "url"])?;
            // CppNix elides the `tarball+` scheme tag if the URL
            // already starts with http:// or https://.
            if (url.starts_with("http://") || url.starts_with("https://")) && qs.is_empty() {
                Ok(Value::string(url))
            } else {
                Ok(Value::string(format!("tarball+{url}{qs}")))
            }
        }
        "path" => {
            let path = attrs
                .get("path")
                .ok_or_else(|| EvalError::AttrNotFound("path".into()))?
                .to_str()?;
            let qs = query_string(attrs, &["type", "path"])?;
            Ok(Value::string(format!("path:{path}{qs}")))
        }
        other => Err(EvalError::TypeError(format!(
            "flakeRefToString: unknown flake type '{other}'"
        ))),
    }
}
