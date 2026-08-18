//! Conversion builtins: toJSON, fromJSON, fromTOML, toXML, convertHash, hashFile.

use super::*;
use sui_compat::versions::xml_escape;

pub(crate) fn register(builtins: &mut NixAttrs) {
    register_builtin(builtins, "toJSON", |args| {
        // CppNix `builtins.toJSON` PRESERVES string context: every string
        // (and every derivation-outPath / copy-to-store path) it serializes
        // contributes its store-path references to the OUTPUT string's
        // context. `to_json` (plain) drops context — which silently broke
        // `lib.generators.toLua` (`toJSON v` over a `toString drv`) so the
        // generated luarocks config `.drv` lost its `external_deps_dirs`
        // inputDrvs (neovim's `mpack-luarocks-config.lua`). Thread context
        // through `to_json_with_context` and attach it to the result.
        // This also matches CppNix's stricter type handling (a lambda or a
        // non-store path throws, rather than emitting a placeholder string).
        let mut ctx = crate::value::StringContext::default();
        let json = args[0].to_json_with_context(&mut ctx)?;
        let s = serde_json::to_string(&json)
            .unwrap_or_else(|_| "null".to_string());
        Ok(Value::String(Rc::new(NixString::with_context(s, ctx))))
    });
    register_builtin(builtins, "fromJSON", |args| {
        let s = args[0].as_string()?;
        let json: serde_json::Value = serde_json::from_str(s)
            .map_err(|e| EvalError::TypeError(format!("fromJSON: {e}")))?;
        Ok(json_to_value(&json))
    });

    register_builtin(builtins, "fromTOML", |args| {
        let s = args[0].as_string()?;
        let table: toml::Value = toml::from_str(s)
            .map_err(|e| EvalError::TypeError(format!("fromTOML: {e}")))?;
        Ok(toml_to_value(&table))
    });

    // convertHash
    register_builtin(builtins, "convertHash", |args| {
        use base64::Engine;
        let attrs = args[0].to_attrs()?;
        let hash_str = attrs
            .get("hash")
            .ok_or_else(|| EvalError::AttrNotFound("hash".into()))?
            .to_str()?;
        let to_format = attrs
            .get("toHashFormat")
            .ok_or_else(|| EvalError::AttrNotFound("toHashFormat".into()))?
            .to_str()?;
        let (algo, raw_hash): (String, String) = if let Some(algo_v) =
            attrs.get("hashAlgo")
        {
            (algo_v.to_str()?, hash_str.clone())
        } else if let Some(stripped) = hash_str.strip_prefix("sha256-") {
            ("sha256".to_string(), stripped.to_string())
        } else if let Some(stripped) = hash_str.strip_prefix("sha512-") {
            ("sha512".to_string(), stripped.to_string())
        } else {
            return Err(EvalError::TypeError(
                "convertHash: missing hashAlgo".into(),
            ));
        };
        let expected_len = match algo.as_str() {
            "md5" => 16,
            "sha1" => 20,
            "sha256" => 32,
            "sha512" => 64,
            other => {
                return Err(EvalError::TypeError(format!(
                    "convertHash: unsupported algo {other}"
                )))
            }
        };
        let bytes: Vec<u8> = if raw_hash.len() == expected_len * 2
            && raw_hash.chars().all(|c| c.is_ascii_hexdigit())
        {
            (0..raw_hash.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&raw_hash[i..i + 2], 16))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| EvalError::TypeError(format!("convertHash hex: {e}")))?
        } else if let Ok(b) = sui_compat::store_path::nix_base32_decode(&raw_hash) {
            if expected_len != 20 {
                return Err(EvalError::TypeError(
                    "convertHash: nix32 only supported for 20-byte (sha1) hashes".into(),
                ));
            }
            b.to_vec()
        } else if let Ok(b) = base64::engine::general_purpose::STANDARD.decode(&raw_hash)
        {
            b
        } else {
            return Err(EvalError::TypeError(format!(
                "convertHash: cannot decode hash '{raw_hash}'"
            )));
        };
        if bytes.len() != expected_len {
            return Err(EvalError::TypeError(format!(
                "convertHash: decoded {} bytes, expected {expected_len} for {algo}",
                bytes.len()
            )));
        }
        let out = match to_format.as_str() {
            "base16" => {
                let mut s = String::with_capacity(bytes.len() * 2);
                for b in &bytes {
                    s.push_str(&format!("{b:02x}"));
                }
                s
            }
            "nix32" => {
                if expected_len != 20 {
                    return Err(EvalError::TypeError(
                        "convertHash: nix32 output only supported for 20-byte hashes".into(),
                    ));
                }
                sui_compat::store_path::nix_base32_encode(&bytes)
            }
            "base64" => base64::engine::general_purpose::STANDARD.encode(&bytes),
            "sri" => format!(
                "{algo}-{}",
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            ),
            other => {
                return Err(EvalError::TypeError(format!(
                    "convertHash: unsupported toHashFormat {other}"
                )))
            }
        };
        Ok(Value::string(out))
    });

    // hashFile (curried)
    //
    // ── ★ THE PATH MUST BE REALIZED *AND* MATERIALIZED ────────────────────
    // Both halves, in this order, or this builtin ENOENTs on a path that
    // `pathExists` just confirmed. `coerce_to_realized_path` is IFD (build a
    // derivation argument before reading it); `materialize_str` is the
    // FILESYSTEM-READ-ONLY redirect that rewrites a fetched input's
    // `/nix/store/<narhash>-source` NAME onto the fetcher cache dir where the
    // bytes actually live. A fetched flake input's store path never exists on
    // the real filesystem, so the second half is not an optimisation.
    //
    // Measured 2026-08-17: substrate's D2 freshness gate is
    // `if pathExists p && … then hashFile p`, so the guard passed and this
    // threw, and `sui system rebuild` on every fleet darwin/NixOS config died
    // at `navigate attrs: hashFile: No such file or directory` — identically
    // for a local `.#host` ref and a remote `github:…/<rev>#host` one, which
    // is what proved it was eval, not the fetcher. `pathExists` (paths.rs),
    // `readFile`, `readDir`, `import` and `builtins.path` all already spell
    // both halves; this one spelled neither.
    register_curried(builtins, "hashFile", |algo, path_val| {
        let algo_str = algo.as_string()?;
        let path_str =
            crate::path::materialize_str(&path_val.coerce_to_realized_path("hashFile")?);
        let contents = std::fs::read(&path_str)
            .map_err(|e| EvalError::IoError { context: "hashFile".into(), message: e.to_string() })?;
        let hex = match algo_str {
            "sha256" => {
                use sha2::{Sha256, Digest};
                format!("{:x}", Sha256::digest(&contents))
            }
            "sha512" => {
                use sha2::{Sha512, Digest};
                format!("{:x}", Sha512::digest(&contents))
            }
            _ => return Err(EvalError::TypeError(format!("hashFile: unsupported algorithm: {algo_str}"))),
        };
        Ok(Value::string(hex))
    });

    // toXML — CppNix `printValueAsXML(state, strict=true, location=false, …)`.
    //
    // Three shipped defects this shape exists to prevent:
    //
    //  1. sui emitted `<thunk />` — an element CppNix CANNOT produce — for
    //     every unforced value, yielding a well-formed document that parsed
    //     fine and was wrong. `value_to_xml` therefore renders `Concrete`,
    //     whose enum has NO `Thunk` variant, so that arm is UNWRITABLE rather
    //     than merely unreached. Demanding at each node as we descend is
    //     exactly what CppNix's `strict = true` means.
    //  2. The `<expr>` root wrapper was missing, so EVERY call was wrong —
    //     including the ones whose bodies looked correct. That is why nothing
    //     caught it: all seven existing tests are substring `contains` checks
    //     against the body, and none of them looks at the root.
    //  3. `xml_escape` left LF raw inside an attribute value, where XML
    //     attribute-value normalization turns it into a space on the way back
    //     out. Data loss, not cosmetics — CppNix's XMLWriter emits `&#xA;`.
    register_builtin(builtins, "toXML", |args| {
        let mut drvs_seen: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut ancestors: Vec<usize> = Vec::new();
        let body = value_to_xml(&args[0], 2, &mut drvs_seen, &mut ancestors)?;
        Ok(Value::string(format!(
            "<?xml version='1.0' encoding='utf-8'?>\n<expr>\n{body}\n</expr>\n"
        )))
    });
}

/// `Some((drvPath, outPath))` when this attrset is a derivation — CppNix's
/// `isDerivation` test, which gates the `<derivation>` element.
fn derivation_ids(attrs: &NixAttrs) -> Result<Option<(String, String)>, EvalError> {
    let Some(ty) = attrs.get("type") else {
        return Ok(None);
    };
    let Concrete::String(ty) = ty.demand()? else {
        return Ok(None);
    };
    if &*ty.chars != "derivation" {
        return Ok(None);
    }
    let (Some(drv), Some(out)) = (attrs.get("drvPath"), attrs.get("outPath")) else {
        return Ok(None);
    };
    let Concrete::String(drv) = drv.demand()? else {
        return Ok(None);
    };
    let Concrete::String(out) = out.demand()? else {
        return Ok(None);
    };
    Ok(Some((drv.chars.to_string(), out.chars.to_string())))
}

/// CppNix has NO cycle protection in `printValueAsXML`: `builtins.toXML` on a
/// self-referential value SIGSEGVs (measured 2026-08-18, rc=139). sui refuses
/// with a typed error instead — a segfault and an invented element are both
/// worse than saying so, and there is no parity cost, because the oracle emits
/// no bytes to be unequal to.
///
/// The guard is an ANCESTOR stack, not a seen-set. CppNix re-expands a shared
/// non-cyclic value in full, so a seen-set would break DAG parity:
/// `let a = { n = 1; }; in builtins.toXML { p = a; q = a; }` must render `a`
/// twice. Only a genuine ancestor is a cycle.
fn enter_cycle_guard(ancestors: &mut Vec<usize>, key: usize) -> Result<(), EvalError> {
    if ancestors.contains(&key) {
        return Err(EvalError::InfiniteRecursion(
            "toXML: value contains a cycle".into(),
        ));
    }
    ancestors.push(key);
    Ok(())
}

/// Render one node, byte-for-byte as CppNix's `printValueAsXML` does.
///
/// Takes `&Value` but immediately demands it to [`Concrete`] — see defect (1)
/// on the registration above. Every recursion re-demands, so laziness resolves
/// top-down exactly as `strict = true` prescribes, and a forcing failure
/// propagates out of `toXML` (CppNix does not contain the error either).
fn value_to_xml(
    v: &Value,
    indent: usize,
    drvs_seen: &mut std::collections::HashSet<String>,
    ancestors: &mut Vec<usize>,
) -> Result<String, EvalError> {
    let pad = " ".repeat(indent);
    Ok(match v.demand()? {
        Concrete::Null => format!("{pad}<null />"),
        Concrete::Bool(b) => format!("{pad}<bool value=\"{b}\" />"),
        Concrete::Int(n) => format!("{pad}<int value=\"{n}\" />"),
        // The SHARED cross-engine formatter, never Rust's `{}` Display:
        // `format!("{f}")` renders 3.0e10 as `30000000000` where CppNix
        // (and sui's own value printer) say `3e+10`. Re-deriving a
        // cross-engine fact locally is how the two answers drift.
        Concrete::Float(f) => format!(
            "{pad}<float value=\"{}\" />",
            sui_compat::versions::cppnix_format_float(f)
        ),
        Concrete::String(ns) => {
            format!("{pad}<string value=\"{}\" />", xml_escape(&ns.chars))
        }
        Concrete::Path(p) => format!("{pad}<path value=\"{}\" />", xml_escape(&p)),
        Concrete::List(items) => {
            enter_cycle_guard(ancestors, Rc::as_ptr(&items) as usize)?;
            let mut out = format!("{pad}<list>\n");
            for item in items.iter() {
                out.push_str(&value_to_xml(item, indent + 2, drvs_seen, ancestors)?);
                out.push('\n');
            }
            out.push_str(&format!("{pad}</list>"));
            ancestors.pop();
            out
        }
        Concrete::Attrs(attrs) => {
            let drv = derivation_ids(&attrs)?;
            // CppNix's seen-set is DERIVATIONS ONLY, and `<repeated />` is a
            // CHILD of `<derivation>`, not a replacement for it — the open tag
            // keeps both attributes either way. (Measured; the natural reading
            // of "emits <repeated /> for a seen derivation" is wrong.)
            if let Some((drv_path, out_path)) = &drv
                && !drvs_seen.insert(drv_path.clone())
            {
                return Ok(format!(
                    "{pad}<derivation drvPath=\"{}\" outPath=\"{}\">\n\
                     {pad}  <repeated />\n\
                     {pad}</derivation>",
                    xml_escape(drv_path),
                    xml_escape(out_path)
                ));
            }
            enter_cycle_guard(ancestors, Rc::as_ptr(&attrs) as usize)?;
            let (open, close) = match &drv {
                Some((d, o)) => (
                    format!(
                        "{pad}<derivation drvPath=\"{}\" outPath=\"{}\">\n",
                        xml_escape(d),
                        xml_escape(o)
                    ),
                    format!("{pad}</derivation>"),
                ),
                None => (format!("{pad}<attrs>\n"), format!("{pad}</attrs>")),
            };
            let mut out = open;
            for (k, val) in attrs.iter() {
                out.push_str(&format!("{pad}  <attr name=\"{}\">\n", xml_escape(&k)));
                out.push_str(&value_to_xml(val, indent + 4, drvs_seen, ancestors)?);
                out.push('\n');
                out.push_str(&format!("{pad}  </attr>\n"));
            }
            out.push_str(&close);
            ancestors.pop();
            out
        }
        // CppNix distinguishes THREE function shapes. Collapsing them to a bare
        // `<function />` discarded the parameter names entirely.
        Concrete::Lambda(cl) => {
            let inner = match &cl.param {
                rnix::ast::Param::IdentParam(ip) => ip
                    .ident()
                    .map(|i| {
                        format!(
                            "{pad}  <varpat name=\"{}\" />\n",
                            xml_escape(&crate::eval::ident_text(&i))
                        )
                    })
                    .unwrap_or_default(),
                rnix::ast::Param::Pattern(pat) => {
                    let name = pat
                        .pat_bind()
                        .and_then(|b| b.ident())
                        .map(|i| {
                            format!(
                                " name=\"{}\"",
                                xml_escape(&crate::eval::ident_text(&i))
                            )
                        })
                        .unwrap_or_default();
                    let ellipsis = if pat.ellipsis_token().is_some() {
                        " ellipsis=\"1\""
                    } else {
                        ""
                    };
                    // CppNix writes `ellipsis` BEFORE `name`
                    // (`<attrspat ellipsis="1" name="args">`); the
                    // opposite order is well-formed XML and not
                    // byte-parity, which is the bar here.
                    let mut s = format!("{pad}  <attrspat{ellipsis}{name}>\n");
                    for e in pat.pat_entries() {
                        if let Some(id) = e.ident() {
                            s.push_str(&format!(
                                "{pad}    <attr name=\"{}\" />\n",
                                xml_escape(&crate::eval::ident_text(&id))
                            ));
                        }
                    }
                    s.push_str(&format!("{pad}  </attrspat>\n"));
                    s
                }
            };
            format!("{pad}<function>\n{inner}{pad}</function>")
        }
        // A primop, or a partial application of one, is `<unevaluated />`.
        Concrete::Builtin(_) => format!("{pad}<unevaluated />"),
    })
}
