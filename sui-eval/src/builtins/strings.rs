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
        // `.` MUST MATCH A NEWLINE — CppNix parity, and the difference is a
        // SILENT WRONG ANSWER rather than an error.
        //
        // CppNix compiles with `std::regex::extended` (POSIX ERE), where `.`
        // matches any character INCLUDING `\n`. Rust's `regex` crate defaults
        // the other way, so every pattern of the shape `.*needle.*` returns
        // `null` here for a multi-line subject while CppNix returns a match.
        //
        // Measured 2026-08-09, minimal case:
        //     builtins.match ".*b.*" "a\nb"    nix: [ ]   sui: null
        //     builtins.match ".*b.*" "ab"      both match
        //
        // Blast radius is not niche: nixpkgs implements `lib.hasInfix` as
        // `builtins.match ".*<needle>.*"`, so ANY hasInfix over generated
        // multi-line text — a script body, an ssh config block, an activation
        // snippet — silently answered `false`. It cost 5 wrong assertions in
        // this fleet's own iroha suite (776/781 vs nix's 781/781) with no
        // error anywhere, which is exactly the "wrong value that still
        // evaluates" class sui's own north star ranks worse than a crash.
        let re = regex::RegexBuilder::new(pattern)
            .dot_matches_new_line(true)
            .build()
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
        Ok(Value::String(Rc::new(NixString::with_context(s, ctx))))
    });
    register_builtin(builtins, "stringLength", |args| {
        Ok(Value::Int(args[0].as_string()?.len() as i64))
    });
    register_builtin(builtins, "substring", |args| {
        // Mirror the VM's substring — see sui-bytecode/src/builtins.rs
        // for the rationale. CppNix uses negative length to mean
        // "to end of string" (core idiom in lib.strings.removePrefix);
        // negative start yields an empty string.
        let start_i = args[0].as_int()?;
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "substring<p1>",
            func: Rc::new(move |args2| {
                let len_i = args2[0].as_int()?;
                Ok(Value::Builtin(Box::new(BuiltinFn {
                    name: "substring<p2>",
                    func: Rc::new(move |args3| {
                        // nix: `substring` is context-PRESERVING — the slice
                        // carries the input string's context (a store-path dep
                        // survives a substring). Preserve it for the String
                        // case (verified `nix eval` hasContext=true); a coerced
                        // path arg keeps prior behavior.
                        let (s, ctx) = match &args3[0] {
                            Value::String(ns) => {
                                (ns.chars.to_string(), Some(ns.context.clone()))
                            }
                            _ => (args3[0].as_string()?.to_string(), None),
                        };
                        if start_i < 0 {
                            return Err(EvalError::TypeError(
                                "substring: negative start position".into(),
                            ));
                        }
                        let s_len = s.len();
                        let start = (start_i as usize).min(s_len);
                        let end = if len_i < 0 {
                            s_len
                        } else {
                            start.saturating_add(len_i as usize).min(s_len)
                        };
                        let slice = s[start..end].to_string();
                        match ctx {
                            Some(ctx) => Ok(Value::String(Rc::new(
                                NixString::with_context(slice, ctx),
                            ))),
                            None => Ok(Value::string(slice)),
                        }
                    }),
                })))
            }),
        })))
    });

    // Case conversion (context-preserving)
    register_builtin(builtins, "toLower", |args| {
        let ns = args[0].as_nix_string()?;
        Ok(Value::String(Rc::new(NixString::with_context(
            ns.chars.to_lowercase(),
            ns.context.clone(),
        ))))
    });
    register_builtin(builtins, "toUpper", |args| {
        let ns = args[0].as_nix_string()?;
        Ok(Value::String(Rc::new(NixString::with_context(
            ns.chars.to_uppercase(),
            ns.context.clone(),
        ))))
    });

    register_builtin(builtins, "replaceStrings", |args| {
        let from = args[0].to_list()?.iter()
            .map(|v| v.to_str())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Value::Builtin(Box::new(BuiltinFn {
            name: "replaceStrings<p1>",
            func: Rc::new(move |args2| {
                // Keep each replacement's STRING + its CONTEXT. CppNix's
                // `prim_replaceStrings` accumulates the context of the
                // subject string PLUS the context of every replacement
                // that actually fires — dropping either loses input-drv
                // references and diverges the drv (e.g. `escapeShellArg`
                // uses replaceStrings; a `-B${gcc}/bin` value stopped
                // registering gcc, so `replaceVars`-built patches were
                // missing their toolchain inputDrvs).
                let to: Vec<(String, StringContext)> = args2[0]
                    .to_list()?
                    .iter()
                    .map(|v| {
                        let forced = crate::eval::force_value(v)?;
                        let (s, c) = forced.coerce_to_string()?;
                        Ok::<_, EvalError>((s, c))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let from2 = from.clone();
                Ok(Value::Builtin(Box::new(BuiltinFn {
                    name: "replaceStrings<p2>",
                    func: Rc::new(move |args3| {
                        // Subject string WITH its context.
                        let forced = crate::eval::force_value(&args3[0])?;
                        let (subject, subject_ctx) = forced.coerce_to_string()?;
                        let mut ctx = subject_ctx;

                        // Left-to-right scan matching CppNix: at each
                        // position, try each `from[i]` in order; the first
                        // match wins, emits `to[i]` (accumulating its
                        // context), and advances past the match. Empty
                        // `from` entries match the empty string at every
                        // position (CppNix semantics), so handle them the
                        // same way — but never loop forever.
                        let bytes = subject.as_bytes();
                        let mut result = String::with_capacity(subject.len());
                        let mut i = 0usize;
                        while i < bytes.len() {
                            let mut matched = false;
                            for (idx, f) in from2.iter().enumerate() {
                                if !f.is_empty() && subject[i..].starts_with(f.as_str()) {
                                    result.push_str(&to[idx].0);
                                    ctx.merge(&to[idx].1);
                                    i += f.len();
                                    matched = true;
                                    break;
                                }
                            }
                            if !matched {
                                // CppNix also matches an EMPTY `from` at the
                                // current position before copying the char.
                                if let Some(empty_idx) =
                                    from2.iter().position(|f| f.is_empty())
                                {
                                    result.push_str(&to[empty_idx].0);
                                    ctx.merge(&to[empty_idx].1);
                                }
                                let ch_len = subject[i..]
                                    .chars()
                                    .next()
                                    .map_or(1, char::len_utf8);
                                result.push_str(&subject[i..i + ch_len]);
                                i += ch_len;
                            }
                        }
                        // Trailing empty-`from` match at end-of-string.
                        if let Some(empty_idx) = from2.iter().position(|f| f.is_empty()) {
                            result.push_str(&to[empty_idx].0);
                            ctx.merge(&to[empty_idx].1);
                        }

                        Ok(Value::String(Rc::new(NixString::with_context(
                            result, ctx,
                        ))))
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
                // CppNix's prim_concatStringsSep coerces each element to a
                // string and ACCUMULATES its string context into the result
                // (`v.mkString(res, context)`). Dropping the element context —
                // as the old `to_str()`/`Value::string` path did — loses every
                // input-drv reference a coerced derivation output carried, so a
                // `${pkg.out}/lib` element stopped registering `pkg`'s `out`
                // output. That silently diverged `lib.makeLibraryPath` /
                // xgcc's `LIBRARY_PATH` (zlib requested `["dev"]` not
                // `["dev","out"]`) → a different hashDerivationModulo →
                // divergent drvPath. Merge the context like CppNix.
                let list = args2[0].to_list()?;
                let mut result = String::new();
                let mut ctx = StringContext::new();
                for (i, v) in list.iter().enumerate() {
                    if i > 0 { result.push_str(&sep); }
                    let (s, c) = v.coerce_to_string()?;
                    result.push_str(&s);
                    ctx.merge(&c);
                }
                Ok(Value::String(Rc::new(NixString::with_context(result, ctx))))
            }),
        })))
    });
    register_string_predicate!(builtins, "hasPrefix", "hasPrefix<partial>",
        |prefix: &str, s: &str| s.starts_with(prefix));
    register_string_predicate!(builtins, "hasSuffix", "hasSuffix<partial>",
        |suffix: &str, s: &str| s.ends_with(suffix));

    // concatStrings — concat without separator (= concatStringsSep "").
    // Same context-accumulation contract as concatStringsSep above.
    register_builtin(builtins, "concatStrings", |args| {
        let list = args[0].to_list()?;
        let mut result = String::new();
        let mut ctx = StringContext::new();
        for v in list.iter() {
            let (s, c) = v.coerce_to_string()?;
            result.push_str(&s);
            ctx.merge(&c);
        }
        Ok(Value::String(Rc::new(NixString::with_context(result, ctx))))
    });

    // Regex: hashString, match, split
    register_curried(builtins, "hashString", |algo, s| {
        let algo_str = algo.as_string()?;
        let input = s.as_string()?;
        // CppNix supports md5, sha1, sha256, sha512 at minimum (plus
        // blake3/blake2b/etc. on newer versions via `builtins.convertHash`
        // — which routes elsewhere). Adding the missing three; found
        // by probing `builtins.hashString "md5" "hello"` which CppNix
        // returns `"5d41402abc4b2a76b9719d911017c592"` and sui was
        // rejecting with 'unsupported algorithm'.
        let hex = match algo_str {
            "md5" => {
                use md5::{Md5, Digest};
                format!("{:x}", Md5::digest(input.as_bytes()))
            }
            "sha1" => {
                use sha1::{Sha1, Digest};
                format!("{:x}", Sha1::digest(input.as_bytes()))
            }
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
                Ok(Value::List(Rc::new(NixList::new(groups))))
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
            // Add the match groups as a list.
            // Per Nix spec: when the regex has no capture groups,
            // the separator position gets an empty list [].
            // When it has capture groups, those captures are the list elements.
            if let Some(caps) = re.captures(&input[m.start()..]) {
                let groups: Vec<Value> = (1..caps.len())
                    .map(|i| match caps.get(i) {
                        Some(g) => Value::string(g.as_str()),
                        None => Value::Null,
                    })
                    .collect();
                result.push(Value::List(Rc::new(NixList::new(groups))));
            }
            last_end = m.end();
        }
        // Add trailing non-matching part
        result.push(Value::string(&input[last_end..]));
        Ok(Value::List(Rc::new(NixList::new(result))))
    });
}
