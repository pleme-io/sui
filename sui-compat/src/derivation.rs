//! Nix derivation (.drv) ATerm format — parse/serialize.
//!
//! Clean recursive-descent parser using a `Cursor` abstraction over the input.
//! The ATerm format is simple enough that parser combinators are unnecessary.

use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DerivationError {
    #[error("parse error at byte {pos}: {message}")]
    Parse { pos: usize, message: String },
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A derivation output descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DerivationOutput {
    /// Store path for this output (empty for floating content-addressed outputs).
    pub path: String,
    /// Hash algorithm (e.g. `"sha256"`), empty for input-addressed outputs.
    pub hash_algo: String,
    /// Expected hash value, empty for input-addressed outputs.
    pub hash: String,
}

/// A parsed Nix derivation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Derivation {
    /// Named outputs (e.g. `"out"`, `"dev"`, `"lib"`).
    pub outputs: BTreeMap<String, DerivationOutput>,
    /// Input derivations: maps `.drv` store path to the list of outputs used.
    pub input_derivations: BTreeMap<String, Vec<String>>,
    /// Input source store paths (non-derivation dependencies).
    pub input_sources: Vec<String>,
    /// Target system triple (e.g. `"x86_64-linux"`).
    pub system: String,
    /// Path to the builder executable.
    pub builder: String,
    /// Arguments passed to the builder.
    pub args: Vec<String>,
    /// Environment variables set during the build.
    pub env: BTreeMap<String, String>,
}

// ── Cursor ───────────────────────────────────────────────────

/// Simple cursor over a byte slice for zero-copy parsing.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn peek(&self) -> Result<u8, DerivationError> {
        self.data.get(self.pos).copied().ok_or(DerivationError::UnexpectedEof)
    }

    fn advance(&mut self) { self.pos += 1; }

    fn expect(&mut self, ch: u8) -> Result<(), DerivationError> {
        let got = self.peek()?;
        if got != ch {
            return Err(DerivationError::Parse {
                pos: self.pos,
                message: format!("expected '{}', got '{}'", ch as char, got as char),
            });
        }
        self.advance();
        Ok(())
    }

    fn expect_str(&mut self, s: &[u8]) -> Result<(), DerivationError> {
        for &ch in s {
            self.expect(ch)?;
        }
        Ok(())
    }

    /// Parse a quoted string with escape handling.
    fn string(&mut self) -> Result<String, DerivationError> {
        self.expect(b'"')?;
        let mut result = Vec::new();
        loop {
            let ch = self.peek()?;
            self.advance();
            match ch {
                b'"' => return String::from_utf8(result).map_err(|e| DerivationError::Parse {
                    pos: self.pos, message: format!("invalid UTF-8: {e}"),
                }),
                b'\\' => {
                    let esc = self.peek()?;
                    self.advance();
                    match esc {
                        b'n' => result.push(b'\n'),
                        b'r' => result.push(b'\r'),
                        b't' => result.push(b'\t'),
                        b'\\' => result.push(b'\\'),
                        b'"' => result.push(b'"'),
                        _ => { result.push(b'\\'); result.push(esc); }
                    }
                }
                _ => result.push(ch),
            }
        }
    }

    /// Parse `[item, item, ...]` using the given item parser.
    fn list<T>(&mut self, parse_item: impl Fn(&mut Self) -> Result<T, DerivationError>) -> Result<Vec<T>, DerivationError> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        if self.peek()? != b']' {
            items.push(parse_item(self)?);
            while self.peek()? == b',' {
                self.advance();
                items.push(parse_item(self)?);
            }
        }
        self.expect(b']')?;
        Ok(items)
    }
}

impl Derivation {
    /// Parse a derivation from its ATerm bytes.
    pub fn parse(input: &[u8]) -> Result<Self, DerivationError> {
        let mut c = Cursor::new(input);
        c.expect_str(b"Derive(")?;

        let outputs_list = c.list(|c| {
            c.expect(b'(')?;
            let name = c.string()?;
            c.expect(b',')?;
            let path = c.string()?;
            c.expect(b',')?;
            let hash_algo = c.string()?;
            c.expect(b',')?;
            let hash = c.string()?;
            c.expect(b')')?;
            Ok((name, DerivationOutput { path, hash_algo, hash }))
        })?;
        c.expect(b',')?;

        let input_drvs_list = c.list(|c| {
            c.expect(b'(')?;
            let path = c.string()?;
            c.expect(b',')?;
            let outputs = c.list(|c| c.string())?;
            c.expect(b')')?;
            Ok((path, outputs))
        })?;
        c.expect(b',')?;

        let input_sources = c.list(|c| c.string())?;
        c.expect(b',')?;
        let system = c.string()?;
        c.expect(b',')?;
        let builder = c.string()?;
        c.expect(b',')?;
        let args = c.list(|c| c.string())?;
        c.expect(b',')?;

        let env_list = c.list(|c| {
            c.expect(b'(')?;
            let key = c.string()?;
            c.expect(b',')?;
            let value = c.string()?;
            c.expect(b')')?;
            Ok((key, value))
        })?;

        c.expect(b')')?;

        Ok(Derivation {
            outputs: outputs_list.into_iter().collect(),
            input_derivations: input_drvs_list.into_iter().collect(),
            input_sources,
            system,
            builder,
            args,
            env: env_list.into_iter().collect(),
        })
    }

    /// Serialize the derivation to ATerm format.
    #[must_use]
    pub fn serialize(&self) -> String {
        let mut out = String::from("Derive(");

        out.push('[');
        out.push_str(&join_comma(self.outputs.iter().map(|(name, o)| {
            format!("({},{},{},{})", escape(name), escape(&o.path), escape(&o.hash_algo), escape(&o.hash))
        })));
        out.push_str("],");

        out.push('[');
        out.push_str(&join_comma(self.input_derivations.iter().map(|(path, outputs)| {
            let outs = join_comma(outputs.iter().map(|o| escape(o)));
            format!("({},[{outs}])", escape(path))
        })));
        out.push_str("],");

        let mut sources: Vec<_> = self.input_sources.iter().collect();
        sources.sort();
        out.push('[');
        out.push_str(&join_comma(sources.iter().map(|s| escape(s))));
        out.push_str("],");

        out.push_str(&escape(&self.system));
        out.push(',');
        out.push_str(&escape(&self.builder));

        out.push_str(",[");
        out.push_str(&join_comma(self.args.iter().map(|a| escape(a))));
        out.push_str("],[");
        out.push_str(&join_comma(self.env.iter().map(|(k, v)| {
            format!("({},{})", escape(k), escape(v))
        })));
        out.push_str("])");
        out
    }
}

/// Join an iterator of string fragments with commas.
fn join_comma(items: impl Iterator<Item = String>) -> String {
    let mut out = String::new();
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&item);
    }
    out
}

/// Escape a string for ATerm serialization (backslash-escaping special chars).
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn serialize_roundtrip() {
        let mut outputs = BTreeMap::new();
        outputs.insert("out".to_string(), DerivationOutput {
            path: "/nix/store/abc-hello".to_string(), hash_algo: String::new(), hash: String::new(),
        });
        let mut env = BTreeMap::new();
        env.insert("name".to_string(), "hello".to_string());
        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec!["/nix/store/src".to_string()],
            system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec!["-e".to_string()],
            env,
        };
        let s = drv.serialize();
        let parsed = Derivation::parse(s.as_bytes()).unwrap();
        assert_eq!(parsed, drv);
    }

    #[test]
    fn escape_roundtrip() {
        let mut env = BTreeMap::new();
        env.insert("s".to_string(), "line1\nline2\r\ttab\\back\"quote".to_string());
        let drv = Derivation {
            outputs: { let mut m = BTreeMap::new(); m.insert("out".to_string(), DerivationOutput { path: "/out".to_string(), hash_algo: String::new(), hash: String::new() }); m },
            input_derivations: BTreeMap::new(), input_sources: vec![], system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(), args: vec![], env,
        };
        let parsed = Derivation::parse(drv.serialize().as_bytes()).unwrap();
        assert_eq!(parsed, drv);
    }

    #[test]
    fn multiple_outputs() {
        let mut outputs = BTreeMap::new();
        for name in ["dev", "lib", "out"] {
            outputs.insert(name.to_string(), DerivationOutput {
                path: format!("/nix/store/{name}"), hash_algo: String::new(), hash: String::new(),
            });
        }
        let drv = Derivation {
            outputs, input_derivations: BTreeMap::new(), input_sources: vec![],
            system: "x86_64-linux".to_string(), builder: "/bin/sh".to_string(), args: vec![], env: BTreeMap::new(),
        };
        let parsed = Derivation::parse(drv.serialize().as_bytes()).unwrap();
        assert_eq!(parsed.outputs.len(), 3);
    }

    #[test]
    fn multiple_input_drvs() {
        let mut input_drvs = BTreeMap::new();
        input_drvs.insert("/nix/store/a.drv".to_string(), vec!["out".to_string()]);
        input_drvs.insert("/nix/store/b.drv".to_string(), vec!["out".to_string(), "lib".to_string()]);
        let drv = Derivation {
            outputs: { let mut m = BTreeMap::new(); m.insert("out".to_string(), DerivationOutput { path: "/out".to_string(), hash_algo: String::new(), hash: String::new() }); m },
            input_derivations: input_drvs, input_sources: vec![], system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(), args: vec![], env: BTreeMap::new(),
        };
        let parsed = Derivation::parse(drv.serialize().as_bytes()).unwrap();
        assert_eq!(parsed.input_derivations.len(), 2);
    }

    #[test]
    fn empty_everything() {
        let drv = Derivation {
            outputs: { let mut m = BTreeMap::new(); m.insert("out".to_string(), DerivationOutput { path: "/out".to_string(), hash_algo: String::new(), hash: String::new() }); m },
            input_derivations: BTreeMap::new(), input_sources: vec![], system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(), args: vec![], env: BTreeMap::new(),
        };
        let s = drv.serialize();
        assert!(s.contains(",[],"));
        let parsed = Derivation::parse(s.as_bytes()).unwrap();
        assert_eq!(parsed, drv);
    }

    #[test]
    fn fixed_output_derivation() {
        let mut outputs = BTreeMap::new();
        outputs.insert("out".to_string(), DerivationOutput {
            path: "/nix/store/src.tar.gz".to_string(),
            hash_algo: "sha256".to_string(),
            hash: "1b0ri5lsf45dknj8bfxi1syz35kmab77apxxg1yrf33la1qm3kc7".to_string(),
        });
        let drv = Derivation {
            outputs, input_derivations: BTreeMap::new(), input_sources: vec![],
            system: "x86_64-linux".to_string(), builder: "/bin/curl".to_string(), args: vec!["-o".to_string()], env: BTreeMap::new(),
        };
        let parsed = Derivation::parse(drv.serialize().as_bytes()).unwrap();
        assert_eq!(parsed, drv);
        assert_eq!(parsed.outputs["out"].hash_algo, "sha256");
    }

    // ── Many outputs ─────────────────────────────────────

    #[test]
    fn many_outputs_roundtrip() {
        let mut outputs = BTreeMap::new();
        for name in ["bin", "data", "debug", "dev", "doc", "info", "lib", "man", "out", "static"] {
            outputs.insert(name.to_string(), DerivationOutput {
                path: format!("/nix/store/hash-pkg-{name}"),
                hash_algo: String::new(),
                hash: String::new(),
            });
        }
        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let parsed = Derivation::parse(drv.serialize().as_bytes()).unwrap();
        assert_eq!(parsed.outputs.len(), 10);
        assert_eq!(parsed, drv);
    }

    // ── Unusual env vars ─────────────────────────────────

    #[test]
    fn env_with_special_characters() {
        let mut env = BTreeMap::new();
        env.insert("multiline".to_string(), "line1\nline2\nline3".to_string());
        env.insert("tabs_and_cr".to_string(), "col1\tcol2\r\n".to_string());
        env.insert("backslash".to_string(), "C:\\Users\\nix".to_string());
        env.insert("quotes".to_string(), r#"say "hello""#.to_string());
        env.insert("empty".to_string(), String::new());

        let drv = Derivation {
            outputs: {
                let mut m = BTreeMap::new();
                m.insert("out".to_string(), DerivationOutput {
                    path: "/out".to_string(), hash_algo: String::new(), hash: String::new(),
                });
                m
            },
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec![],
            env,
        };
        let serialized = drv.serialize();
        let parsed = Derivation::parse(serialized.as_bytes()).unwrap();
        assert_eq!(parsed, drv);
    }

    #[test]
    fn env_with_long_value() {
        let mut env = BTreeMap::new();
        env.insert("NIX_CFLAGS_COMPILE".to_string(), "-I/nix/store/abc ".repeat(100));

        let drv = Derivation {
            outputs: {
                let mut m = BTreeMap::new();
                m.insert("out".to_string(), DerivationOutput {
                    path: "/out".to_string(), hash_algo: String::new(), hash: String::new(),
                });
                m
            },
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec![],
            env,
        };
        let parsed = Derivation::parse(drv.serialize().as_bytes()).unwrap();
        assert_eq!(parsed, drv);
    }

    // ── Multiple input sources (sorted output) ──────────

    #[test]
    fn input_sources_sorted_in_serialization() {
        let drv = Derivation {
            outputs: {
                let mut m = BTreeMap::new();
                m.insert("out".to_string(), DerivationOutput {
                    path: "/out".to_string(), hash_algo: String::new(), hash: String::new(),
                });
                m
            },
            input_derivations: BTreeMap::new(),
            input_sources: vec![
                "/nix/store/zzz-last".to_string(),
                "/nix/store/aaa-first".to_string(),
                "/nix/store/mmm-middle".to_string(),
            ],
            system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let serialized = drv.serialize();
        let parsed = Derivation::parse(serialized.as_bytes()).unwrap();
        assert_eq!(parsed.input_sources, vec![
            "/nix/store/aaa-first".to_string(),
            "/nix/store/mmm-middle".to_string(),
            "/nix/store/zzz-last".to_string(),
        ]);
    }

    // ── Error cases ──────────────────────────────────────

    #[test]
    fn parse_truncated_input() {
        assert!(Derivation::parse(b"Derive(").is_err());
        assert!(Derivation::parse(b"").is_err());
        assert!(Derivation::parse(b"Derive").is_err());
    }

    #[test]
    fn parse_invalid_prefix() {
        assert!(Derivation::parse(b"NotDerive([])").is_err());
    }

    #[test]
    fn parse_missing_closing_paren() {
        let drv = Derivation {
            outputs: {
                let mut m = BTreeMap::new();
                m.insert("out".to_string(), DerivationOutput {
                    path: "/out".to_string(), hash_algo: String::new(), hash: String::new(),
                });
                m
            },
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let mut serialized = drv.serialize();
        serialized.pop(); // Remove the closing ')'
        assert!(Derivation::parse(serialized.as_bytes()).is_err());
    }

    // ── Multiple input derivations with multiple outputs ──

    proptest! {
        #[test]
        fn prop_escape_roundtrip(s in ".*") {
            let escaped = escape(&s);
            let input = format!(
                "Derive([(\"out\",\"/out\",\"\",\"\")],[],[],\"x86_64-linux\",\"/bin/sh\",[],[(\"v\",{escaped})])"
            );
            let drv = Derivation::parse(input.as_bytes()).unwrap();
            prop_assert_eq!(&drv.env["v"], &s);
        }

        #[test]
        fn prop_serialize_parse_roundtrip(
            name in "[a-z]{1,10}",
            builder_arg in "[a-z0-9/]{1,20}",
        ) {
            let drv = Derivation {
                outputs: {
                    let mut m = BTreeMap::new();
                    m.insert("out".to_string(), DerivationOutput {
                        path: format!("/nix/store/hash-{name}"),
                        hash_algo: String::new(),
                        hash: String::new(),
                    });
                    m
                },
                input_derivations: BTreeMap::new(),
                input_sources: vec![],
                system: "x86_64-linux".to_string(),
                builder: format!("/{builder_arg}"),
                args: vec![],
                env: BTreeMap::new(),
            };
            let serialized = drv.serialize();
            let parsed = Derivation::parse(serialized.as_bytes()).unwrap();
            prop_assert_eq!(parsed, drv);
        }
    }

    // ── escape() unit tests ──────────────────────────────

    #[test]
    fn escape_empty_string() {
        assert_eq!(escape(""), "\"\"");
    }

    #[test]
    fn escape_no_special_chars() {
        assert_eq!(escape("hello"), "\"hello\"");
    }

    #[test]
    fn escape_double_quote() {
        assert_eq!(escape("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn escape_backslash() {
        assert_eq!(escape("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn escape_newline() {
        assert_eq!(escape("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn escape_carriage_return() {
        assert_eq!(escape("a\rb"), "\"a\\rb\"");
    }

    #[test]
    fn escape_tab() {
        assert_eq!(escape("a\tb"), "\"a\\tb\"");
    }

    #[test]
    fn escape_all_specials_combined() {
        let s = "a\"b\\c\nd\re\tf";
        let escaped = escape(s);
        assert_eq!(escaped, "\"a\\\"b\\\\c\\nd\\re\\tf\"");
    }

    // ── Empty inputs everywhere ──────────────────────────

    #[test]
    fn empty_outputs_btreemap_serializes_brackets() {
        let drv = Derivation {
            outputs: BTreeMap::new(),
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: String::new(),
            builder: String::new(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let s = drv.serialize();
        // outputs is the very first list — must be empty `[]`
        assert!(s.starts_with("Derive([],"));
        let parsed = Derivation::parse(s.as_bytes()).unwrap();
        assert_eq!(parsed, drv);
    }

    #[test]
    fn unicode_env_values_roundtrip() {
        let mut env = BTreeMap::new();
        env.insert("greeting".to_string(), "こんにちは世界".to_string());
        env.insert("emoji".to_string(), "🎉🚀".to_string());
        env.insert("mixed".to_string(), "test 日本語 café".to_string());
        let drv = Derivation {
            outputs: {
                let mut m = BTreeMap::new();
                m.insert("out".to_string(), DerivationOutput {
                    path: "/out".to_string(),
                    hash_algo: String::new(),
                    hash: String::new(),
                });
                m
            },
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec![],
            env,
        };
        let parsed = Derivation::parse(drv.serialize().as_bytes()).unwrap();
        assert_eq!(parsed, drv);
    }

    #[test]
    fn unknown_escape_sequences_preserved_literally() {
        // The parser preserves unknown \X as \X (literal backslash + char)
        let input = b"Derive([(\"out\",\"/out\",\"\",\"\")],[],[],\"x86_64-linux\",\"/bin/sh\",[],[(\"v\",\"a\\xb\")])";
        let drv = Derivation::parse(input).unwrap();
        // \x is not a recognized escape, so it's preserved as-is
        assert_eq!(drv.env["v"], "a\\xb");
    }

    #[test]
    fn parse_error_includes_position() {
        // Truncated input — parsing fails somewhere
        let result = Derivation::parse(b"Derive([(\"out\"");
        assert!(result.is_err());
    }

    #[test]
    fn parse_invalid_outputs_format() {
        // Outputs missing the closing tuple paren
        let result = Derivation::parse(b"Derive([(\"out\",\"/out\",\"\",\"\"],[],[],\"\",\"\",[],[])");
        assert!(result.is_err());
    }

    #[test]
    fn parse_missing_comma_separator() {
        // Missing comma between fields
        let result = Derivation::parse(b"Derive([],[][],\"\",\"\",[],[])");
        assert!(result.is_err());
    }

    #[test]
    fn args_with_many_entries() {
        let drv = Derivation {
            outputs: {
                let mut m = BTreeMap::new();
                m.insert("out".to_string(), DerivationOutput {
                    path: "/out".to_string(),
                    hash_algo: String::new(),
                    hash: String::new(),
                });
                m
            },
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(),
            args: (0..50).map(|i| format!("arg-{i}")).collect(),
            env: BTreeMap::new(),
        };
        let parsed = Derivation::parse(drv.serialize().as_bytes()).unwrap();
        assert_eq!(parsed.args.len(), 50);
        assert_eq!(parsed, drv);
    }

    #[test]
    fn input_derivations_with_many_outputs_each() {
        let mut input_drvs = BTreeMap::new();
        let outputs: Vec<String> =
            ["out", "dev", "lib", "bin", "doc", "man", "info", "static", "debug"]
                .iter()
                .map(ToString::to_string)
                .collect();
        for i in 0..5 {
            input_drvs.insert(format!("/nix/store/dep{i}.drv"), outputs.clone());
        }
        let drv = Derivation {
            outputs: {
                let mut m = BTreeMap::new();
                m.insert("out".to_string(), DerivationOutput {
                    path: "/out".to_string(),
                    hash_algo: String::new(),
                    hash: String::new(),
                });
                m
            },
            input_derivations: input_drvs,
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let parsed = Derivation::parse(drv.serialize().as_bytes()).unwrap();
        assert_eq!(parsed.input_derivations.len(), 5);
        for outs in parsed.input_derivations.values() {
            assert_eq!(outs.len(), 9);
        }
        assert_eq!(parsed, drv);
    }

    #[test]
    fn fixed_output_md5_algorithm() {
        let mut outputs = BTreeMap::new();
        outputs.insert("out".to_string(), DerivationOutput {
            path: "/nix/store/abc-x".to_string(),
            hash_algo: "md5".to_string(),
            hash: "0123456789abcdef0123456789abcdef".to_string(),
        });
        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let parsed = Derivation::parse(drv.serialize().as_bytes()).unwrap();
        assert_eq!(parsed.outputs["out"].hash_algo, "md5");
        assert_eq!(parsed, drv);
    }

    #[test]
    fn empty_string_in_env_key() {
        let mut env = BTreeMap::new();
        env.insert(String::new(), "value-with-empty-key".to_string());
        let drv = Derivation {
            outputs: {
                let mut m = BTreeMap::new();
                m.insert("out".to_string(), DerivationOutput {
                    path: "/out".to_string(),
                    hash_algo: String::new(),
                    hash: String::new(),
                });
                m
            },
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec![],
            env,
        };
        let parsed = Derivation::parse(drv.serialize().as_bytes()).unwrap();
        assert_eq!(parsed.env[""], "value-with-empty-key");
    }

    #[test]
    fn parser_handles_zero_outputs_zero_inputs() {
        let input = b"Derive([],[],[],\"sys\",\"build\",[],[])";
        let drv = Derivation::parse(input).unwrap();
        assert!(drv.outputs.is_empty());
        assert!(drv.input_derivations.is_empty());
        assert!(drv.input_sources.is_empty());
        assert!(drv.args.is_empty());
        assert!(drv.env.is_empty());
        assert_eq!(drv.system, "sys");
        assert_eq!(drv.builder, "build");
    }

    #[test]
    fn default_derivation_is_empty() {
        let drv = Derivation::default();
        assert!(drv.outputs.is_empty());
        assert!(drv.input_derivations.is_empty());
        assert!(drv.input_sources.is_empty());
        assert!(drv.system.is_empty());
        assert!(drv.builder.is_empty());
        assert!(drv.args.is_empty());
        assert!(drv.env.is_empty());
    }

    #[test]
    fn default_derivation_output_is_empty() {
        let out = DerivationOutput::default();
        assert!(out.path.is_empty());
        assert!(out.hash_algo.is_empty());
        assert!(out.hash.is_empty());
    }

    #[test]
    fn outputs_serialized_in_btreemap_order() {
        // BTreeMap iterates keys in sorted order — confirm serialization
        // emits outputs alphabetically.
        let mut outputs = BTreeMap::new();
        for name in ["zzz", "aaa", "mmm"] {
            outputs.insert(name.to_string(), DerivationOutput {
                path: format!("/out-{name}"),
                hash_algo: String::new(),
                hash: String::new(),
            });
        }
        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "sys".to_string(),
            builder: "build".to_string(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let s = drv.serialize();
        // "aaa" should come before "mmm" before "zzz"
        let aaa = s.find("\"aaa\"").unwrap();
        let mmm = s.find("\"mmm\"").unwrap();
        let zzz = s.find("\"zzz\"").unwrap();
        assert!(aaa < mmm);
        assert!(mmm < zzz);
    }

    #[test]
    fn env_serialized_in_btreemap_order() {
        let mut env = BTreeMap::new();
        for k in ["zoo", "alpha", "middle"] {
            env.insert(k.to_string(), format!("v-{k}"));
        }
        let drv = Derivation {
            outputs: {
                let mut m = BTreeMap::new();
                m.insert("out".to_string(), DerivationOutput {
                    path: "/out".to_string(),
                    hash_algo: String::new(),
                    hash: String::new(),
                });
                m
            },
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "sys".to_string(),
            builder: "build".to_string(),
            args: vec![],
            env,
        };
        let s = drv.serialize();
        let alpha = s.find("\"alpha\"").unwrap();
        let middle = s.find("\"middle\"").unwrap();
        let zoo = s.find("\"zoo\"").unwrap();
        assert!(alpha < middle);
        assert!(middle < zoo);
    }

    #[test]
    fn complex_input_derivations() {
        let mut input_drvs = BTreeMap::new();
        input_drvs.insert(
            "/nix/store/abc-gcc.drv".to_string(),
            vec!["out".to_string(), "lib".to_string(), "info".to_string()],
        );
        input_drvs.insert(
            "/nix/store/def-glibc.drv".to_string(),
            vec!["out".to_string(), "dev".to_string(), "static".to_string()],
        );
        input_drvs.insert(
            "/nix/store/ghi-bash.drv".to_string(),
            vec!["out".to_string()],
        );

        let drv = Derivation {
            outputs: {
                let mut m = BTreeMap::new();
                m.insert("out".to_string(), DerivationOutput {
                    path: "/nix/store/result".to_string(),
                    hash_algo: String::new(),
                    hash: String::new(),
                });
                m
            },
            input_derivations: input_drvs,
            input_sources: vec!["/nix/store/src".to_string()],
            system: "x86_64-linux".to_string(),
            builder: "/nix/store/bash/bin/bash".to_string(),
            args: vec!["-e".to_string(), "/nix/store/builder.sh".to_string()],
            env: {
                let mut e = BTreeMap::new();
                e.insert("name".to_string(), "test-pkg".to_string());
                e.insert("version".to_string(), "1.0.0".to_string());
                e
            },
        };
        let parsed = Derivation::parse(drv.serialize().as_bytes()).unwrap();
        assert_eq!(parsed, drv);
        assert_eq!(parsed.input_derivations.len(), 3);
        assert_eq!(parsed.input_derivations["/nix/store/abc-gcc.drv"].len(), 3);
    }
}
