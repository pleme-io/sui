//! `${VAR}` environment-variable expansion for config-file text.
//!
//! Lets a secret-sourced connection string — e.g. a CloudNativePG password
//! injected into the pod as a `secretKeyRef` env var — live inside a
//! ConfigMap-rendered `backend.json` as a `${SUI_CACHE_PG_PASSWORD}` token, so
//! the secret value itself never appears in the ConfigMap. Applied to the whole
//! config-file text *before* it is parsed as TOML/JSON, so it expands a
//! top-level DSN and every nested tier's DSN alike.
//!
//! Semantics are deliberately narrow, so a file with no valid `${VAR}` token
//! round-trips byte-identical — existing password-free (trust-auth) configs and
//! every existing parse test are unchanged:
//!
//! - `${NAME}`, where `NAME` matches `[A-Za-z_][A-Za-z0-9_]*`, is replaced by
//!   `std::env::var("NAME")`. A **missing** variable is a hard
//!   [`ExpandEnvError::Missing`], never a silent empty string — a missing
//!   password must fail loudly, not degrade a DSN to no-auth (which the network
//!   would then either reject noisily or, worse, accept silently).
//! - `$${` is an escape producing a literal `${`.
//! - Any other `$` (bare `$`, or a `${` that does not close on a valid name) is
//!   left exactly as written.

use std::env;

/// A `${VAR}` token referenced an environment variable that is not set.
#[derive(Debug, thiserror::Error)]
pub enum ExpandEnvError {
    /// The named variable was referenced by a `${VAR}` token but is unset.
    #[error("config references ${{{name}}} but environment variable `{name}` is not set")]
    Missing {
        /// The referenced variable name.
        name: String,
    },
}

/// Expand `${VAR}` tokens in `text` against the process environment.
///
/// See the [module docs](self) for the exact semantics. Returns the expanded
/// string, or [`ExpandEnvError::Missing`] on the first unresolved `${VAR}`.
pub fn expand_env_vars(text: &str) -> Result<String, ExpandEnvError> {
    expand_with(text, |name| {
        env::var(name).map_err(|_| ExpandEnvError::Missing {
            name: name.to_string(),
        })
    })
}

/// Core expansion, parameterized by a resolver so tests exercise the parser
/// without mutating the real (process-global, test-race-prone) environment.
fn expand_with<F>(text: &str, mut lookup: F) -> Result<String, ExpandEnvError>
where
    F: FnMut(&str) -> Result<String, ExpandEnvError>,
{
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            // `$${` → literal `${`
            if bytes.get(i + 1) == Some(&b'$') && bytes.get(i + 2) == Some(&b'{') {
                out.push_str("${");
                i += 3;
                continue;
            }
            // `${NAME}` → env value
            if bytes.get(i + 1) == Some(&b'{') {
                if let Some(close) = text[i + 2..].bytes().position(|b| b == b'}') {
                    let name = &text[i + 2..i + 2 + close];
                    if is_valid_name(name) {
                        out.push_str(&lookup(name)?);
                        i = i + 2 + close + 1; // resume past the '}'
                        continue;
                    }
                }
            }
        }
        // Default: copy one whole UTF-8 char. `i` is always on a char boundary
        // here — every special-case branch above advances past ASCII bytes only.
        let ch = text[i..].chars().next().expect("i on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok(out)
}

fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A closure resolver over a fixed table — never touches the real env.
    fn table<'a>(
        pairs: &'a [(&'a str, &'a str)],
    ) -> impl FnMut(&str) -> Result<String, ExpandEnvError> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
                .ok_or_else(|| ExpandEnvError::Missing {
                    name: name.to_string(),
                })
        }
    }

    #[test]
    fn no_tokens_roundtrips_byte_identical() {
        // The current trust-auth DSN — must be returned unchanged.
        let dsn = r#"{"type":"pg","url":"postgres://sui@sui-cache-pg:5432/suicache","max_conns":8}"#;
        assert_eq!(expand_with(dsn, table(&[])).unwrap(), dsn);
    }

    #[test]
    fn expands_a_dsn_user_and_password() {
        let tmpl = "postgres://${U}:${P}@h:5432/db";
        let got = expand_with(tmpl, table(&[("U", "sui"), ("P", "s3cr3t")])).unwrap();
        assert_eq!(got, "postgres://sui:s3cr3t@h:5432/db");
    }

    #[test]
    fn missing_var_is_a_hard_error_never_empty() {
        // A missing password must fail loudly, not produce `postgres://sui:@h/db`.
        let err = expand_with("postgres://${U}:${P}@h/db", table(&[("U", "sui")])).unwrap_err();
        let ExpandEnvError::Missing { name } = err;
        assert_eq!(name, "P");
    }

    #[test]
    fn double_dollar_brace_is_a_literal() {
        assert_eq!(
            expand_with("$${NOT_A_VAR}", table(&[])).unwrap(),
            "${NOT_A_VAR}"
        );
    }

    #[test]
    fn bare_dollar_and_invalid_ref_left_untouched() {
        let s = "cost is $5 and ${bad name} and ${}";
        assert_eq!(expand_with(s, table(&[])).unwrap(), s);
    }

    #[test]
    fn expands_inside_a_tiered_json_l2_url() {
        let json = r#"{"type":"tiered","l2":{"type":"pg","url":"postgres://${U}:${P}@sui-cache-pg-rw:5432/suicache","max_conns":8}}"#;
        let got = expand_with(json, table(&[("U", "sui"), ("P", "pw")])).unwrap();
        assert!(got.contains("postgres://sui:pw@sui-cache-pg-rw:5432/suicache"));
        // Structure (the JSON scaffold) is otherwise untouched.
        assert!(got.starts_with(r#"{"type":"tiered","l2":{"type":"pg","url":"postgres://"#));
    }

    #[test]
    fn redis_password_only_dsn() {
        let got = expand_with("redis://:${RP}@sui-cache-redis:6379", table(&[("RP", "rpw")])).unwrap();
        assert_eq!(got, "redis://:rpw@sui-cache-redis:6379");
    }
}
