//! Bridge from `builtins.sui.narinfo.*` to `sui_spec::narinfo`.

use std::rc::Rc;

use super::*;
use super::bridge_helpers::{
    as_string, as_attrs, attrs_optional_string, attrs_required_string, load_format,
};
use sui_spec::narinfo::{self, NarinfoFormat};

const NAME: &str = "builtins.sui.narinfo";
const FORMAT: &str = "cppnix-narinfo-v1";

pub(crate) fn register(sui_ext: &mut NixAttrs) {
    let mut set = NixAttrs::new();

    register_builtin(&mut set, "parse", |args| {
        let bridge = format!("{NAME}.parse");
        let text = as_string(&args[0], &bridge)?;
        let fmt: NarinfoFormat = load_format(FORMAT, &bridge)?;
        let record = narinfo::parse(&text, &fmt)
            .map_err(|e| EvalError::type_error(format!("{bridge}: {e:?}")))?;
        Ok(record_to_value(&record))
    });

    register_builtin(&mut set, "emit", |args| {
        let bridge = format!("{NAME}.emit");
        let attrs = as_attrs(&args[0], &bridge)?;
        let record = value_to_record(&attrs, &bridge)?;
        Ok(Value::string(narinfo::emit(&record)))
    });

    sui_ext.insert("narinfo".to_string(), Value::Attrs(Rc::new(set)));
}

fn record_to_value(rec: &narinfo::ParsedNarInfo) -> Value {
    let mut out = NixAttrs::new();
    out.insert("storePath".to_string(), Value::string(&rec.store_path));
    out.insert("url".to_string(), Value::string(&rec.url));
    out.insert("compression".to_string(), Value::string(&rec.compression));
    if let Some(h) = &rec.file_hash {
        out.insert("fileHash".to_string(), Value::string(h));
    }
    if let Some(s) = rec.file_size {
        out.insert("fileSize".to_string(), Value::Int(s as i64));
    }
    out.insert("narHash".to_string(), Value::string(&rec.nar_hash));
    out.insert("narSize".to_string(), Value::Int(rec.nar_size as i64));
    out.insert("references".to_string(), Value::list(
        rec.references.iter().map(|s| Value::string(s)).collect(),
    ));
    if let Some(d) = &rec.deriver {
        out.insert("deriver".to_string(), Value::string(d));
    }
    if let Some(s) = &rec.system {
        out.insert("system".to_string(), Value::string(s));
    }
    out.insert("signatures".to_string(), Value::list(
        rec.signatures.iter().map(|s| Value::string(s)).collect(),
    ));
    if let Some(ca) = &rec.ca {
        out.insert("ca".to_string(), Value::string(ca));
    }
    Value::Attrs(Rc::new(out))
}

fn value_to_record(attrs: &NixAttrs, bridge: &str) -> Result<narinfo::ParsedNarInfo, EvalError> {
    Ok(narinfo::ParsedNarInfo {
        store_path: attrs_required_string(attrs, "storePath", bridge)?,
        url: attrs_required_string(attrs, "url", bridge)?,
        compression: attrs_required_string(attrs, "compression", bridge)?,
        file_hash: attrs_optional_string(attrs, "fileHash", bridge)?,
        file_size: match attrs.get("fileSize") {
            Some(v) => match crate::eval::force_value(v)? {
                Value::Int(n) if n >= 0 => Some(n as u64),
                _ => None,
            },
            None => None,
        },
        nar_hash: attrs_required_string(attrs, "narHash", bridge)?,
        nar_size: match attrs.get("narSize") {
            Some(v) => match crate::eval::force_value(v)? {
                Value::Int(n) if n >= 0 => n as u64,
                _ => 0,
            },
            None => 0,
        },
        references: match attrs.get("references") {
            Some(v) => match crate::eval::force_value(v)? {
                Value::List(l) => {
                    let mut refs = Vec::with_capacity(l.len());
                    for item in l.iter() {
                        refs.push(as_string(item, bridge)?);
                    }
                    refs
                }
                _ => Vec::new(),
            },
            None => Vec::new(),
        },
        deriver: attrs_optional_string(attrs, "deriver", bridge)?,
        system: attrs_optional_string(attrs, "system", bridge)?,
        signatures: match attrs.get("signatures") {
            Some(v) => match crate::eval::force_value(v)? {
                Value::List(l) => {
                    let mut sigs = Vec::with_capacity(l.len());
                    for item in l.iter() {
                        sigs.push(as_string(item, bridge)?);
                    }
                    sigs
                }
                _ => Vec::new(),
            },
            None => Vec::new(),
        },
        ca: attrs_optional_string(attrs, "ca", bridge)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
StorePath: /nix/store/abc-hello
URL: nar/abc.nar.xz
Compression: xz
NarHash: sha256:nnn
NarSize: 1024
References: /nix/store/dep1 /nix/store/dep2
Sig: cache.nixos.org-1:sig
";

    #[test]
    fn parse_then_emit_roundtrips() {
        let fmt: NarinfoFormat = load_format(FORMAT, "test").unwrap();
        let record = narinfo::parse(SAMPLE, &fmt).unwrap();
        let v = record_to_value(&record);
        let attrs = match v {
            Value::Attrs(a) => a,
            _ => panic!("expected attrs"),
        };
        match attrs.get("storePath") {
            Some(Value::String(s)) => assert_eq!(s.chars.as_str(), "/nix/store/abc-hello"),
            _ => panic!("expected storePath"),
        }
        match attrs.get("narSize") {
            Some(Value::Int(1024)) => {}
            other => panic!("expected narSize=1024, got {other:?}"),
        }
        match attrs.get("references") {
            Some(Value::List(l)) => assert_eq!(l.len(), 2),
            _ => panic!("expected references list"),
        }
        match attrs.get("signatures") {
            Some(Value::List(l)) => assert_eq!(l.len(), 1),
            _ => panic!("expected signatures list"),
        }
    }
}
