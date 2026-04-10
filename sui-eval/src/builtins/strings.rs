//! String builtins: stringLength, substring, replaceStrings, hasPrefix, hasSuffix,
//! toLower, toUpper, match, split, concatStringsSep, concatStrings.

use std::cell::RefCell;
use std::collections::HashMap;

use super::*;

thread_local! {
    /// LRU-ish regex cache: avoids recompiling the same pattern on every
    /// `builtins.match` / `builtins.split` call.  Nix expressions like
    /// `map (builtins.match "([0-9]+)") list` hit the same pattern
    /// thousands of times during nixpkgs evaluation.
    static REGEX_CACHE: RefCell<HashMap<String, regex::Regex>> = RefCell::new(HashMap::new());
}

fn cached_regex(pattern: &str) -> Result<regex::Regex, EvalError> {
    REGEX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(re) = cache.get(pattern) {
            return Ok(re.clone());
        }
        let re = regex::Regex::new(pattern)
            .map_err(|e| EvalError::TypeError(format!("invalid regex '{pattern}': {e}")))?;
        cache.insert(pattern.to_string(), re.clone());
        Ok(re)
    })
}

/// Register a curried string predicate builtin (first arg is a pattern, second is the subject).
macro_rules! register_string_predicate {
    ($builtins:expr, $name:expr, $partial_name:expr, $check:expr) => {
        register_builtin($builtins, $name, |args| {
            let pattern = args[0].as_string()?.to_string();
            Ok(Value::Builtin(Box::new(BuiltinFn {
                name: $partial_name,
                func: Rc::new(move |args2| {
                    let s = args2[0].as_string()?;
                    Ok(Value::Bool($check(&pattern, s)))
                }),
            })))
        });
    };
}

pub(crate) fn register(builtins: &mut NixAttrs) {
    register_builtin(builtins, "toString", |args| {
        let val = &args[0];
        let (s, ctx) = val.coerce_to_string()?;
        Ok(Value::String(Box::new(NixString::with_context(s, ctx))))
    });
    register_builtin(builtins, "stringLength", |args| {
        Ok(Value::Int(args[0].as_string()?.len() as i64))
    });
    register_builtin(builtins, "substring", |args| {
        let start = args[0].as_int()? as usize;
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "substring<p1>",
            func: Rc::new(move |args2| {
                let len = args2[0].as_int()? as usize;
                Ok(Value::Builtin(Box::new(BuiltinFn {
                    name: "substring<p2>",
                    func: Rc::new(move |args3| {
                        let s = args3[0].as_string()?;
                        let end = (start + len).min(s.len());
                        let start = start.min(s.len());
                        Ok(Value::string(&s[start..end]))
                    }),
                })))
            }),
        })))
    });

    // Case conversion (context-preserving)
    register_builtin(builtins, "toLower", |args| {
        let ns = args[0].as_nix_string()?;
        Ok(Value::String(Box::new(NixString::with_context(
            ns.chars.to_lowercase(),
            ns.context.clone(),
        ))))
    });
    register_builtin(builtins, "toUpper", |args| {
        let ns = args[0].as_nix_string()?;
        Ok(Value::String(Box::new(NixString::with_context(
            ns.chars.to_uppercase(),
            ns.context.clone(),
        ))))
    });

    register_builtin(builtins, "replaceStrings", |args| {
        let from = args[0].as_list()?.iter()
            .map(|v| v.as_string().map(|s| s.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "replaceStrings<p1>",
            func: Rc::new(move |args2| {
                let to = args2[0].as_list()?.iter()
                    .map(|v| v.as_string().map(|s| s.to_string()))
                    .collect::<Result<Vec<_>, _>>()?;
                let from2 = from.clone();
                Ok(Value::Builtin(Box::new(BuiltinFn {
                    name: "replaceStrings<p2>",
                    func: Rc::new(move |args3| {
                        let mut s = args3[0].as_string()?.to_string();
                        for (f, t) in from2.iter().zip(to.iter()) {
                            if !f.is_empty() {
                                s = s.replace(f.as_str(), t);
                            }
                        }
                        Ok(Value::string(s))
                    }),
                })))
            }),
        })))
    });
    register_builtin(builtins, "concatStringsSep", |args| {
        let sep = args[0].as_string()?.to_string();
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "concatStringsSep<partial>",
            func: Rc::new(move |args2| {
                let list = args2[0].as_list()?;
                let strings: Result<Vec<_>, _> = list.iter()
                    .map(|v| v.as_string().map(|s| s.to_string()))
                    .collect();
                Ok(Value::string(strings?.join(&sep)))
            }),
        })))
    });
    register_string_predicate!(builtins, "hasPrefix", "hasPrefix<partial>",
        |prefix: &str, s: &str| s.starts_with(prefix));
    register_string_predicate!(builtins, "hasSuffix", "hasSuffix<partial>",
        |suffix: &str, s: &str| s.ends_with(suffix));

    // concatStrings — concat without separator
    register_builtin(builtins, "concatStrings", |args| {
        let list = args[0].as_list()?;
        let result: Result<String, _> = list.iter()
            .map(|v| v.as_string())
            .collect();
        Ok(Value::string(result?))
    });

    // Regex: hashString, match, split
    register_curried(builtins, "hashString", |algo, s| {
        let algo_str = algo.as_string()?;
        let input = s.as_string()?;
        let hex = match algo_str {
            "sha256" => {
                use sha2::{Sha256, Digest};
                format!("{:x}", Sha256::digest(input.as_bytes()))
            }
            "sha512" => {
                use sha2::{Sha512, Digest};
                format!("{:x}", Sha512::digest(input.as_bytes()))
            }
            _ => return Err(EvalError::TypeError(format!("hashString: unsupported algorithm: {algo_str}"))),
        };
        Ok(Value::string(hex))
    });

    register_curried(builtins, "match", |pattern, s| {
        let pat = pattern.as_string()?;
        let input = s.as_string()?;
        let re = cached_regex(&format!("^{pat}$"))?;
        match re.captures(input) {
            Some(caps) => {
                let groups: Vec<Value> = (1..caps.len())
                    .map(|i| match caps.get(i) {
                        Some(m) => Value::string(m.as_str()),
                        None => Value::Null,
                    })
                    .collect();
                Ok(Value::List(Rc::new(groups)))
            }
            None => Ok(Value::Null),
        }
    });

    // Regex-based split per Nix spec: alternates non-match strings and match group lists.
    register_curried(builtins, "split", |pattern, s| {
        let pat = pattern.as_string()?;
        let input = s.as_string()?;
        let re = cached_regex(pat)?;
        let mut result: Vec<Value> = Vec::new();
        let mut last_end = 0;
        for m in re.find_iter(input) {
            // Add the non-matching part before this match
            result.push(Value::string(&input[last_end..m.start()]));
            // Add the match groups as a list
            if let Some(caps) = re.captures(&input[m.start()..]) {
                let groups: Vec<Value> = (1..caps.len())
                    .map(|i| match caps.get(i) {
                        Some(g) => Value::string(g.as_str()),
                        None => Value::Null,
                    })
                    .collect();
                // If no capture groups, wrap the whole match in a list
                if groups.is_empty() {
                    result.push(Value::List(Rc::new(vec![Value::string(m.as_str())])));
                } else {
                    result.push(Value::List(Rc::new(groups)));
                }
            }
            last_end = m.end();
        }
        // Add trailing non-matching part
        result.push(Value::string(&input[last_end..]));
        Ok(Value::List(Rc::new(result)))
    });
}
