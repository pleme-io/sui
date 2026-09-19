//! The workspace resolves with no C TLS stack: rustls only.
//!
//! # The defect
//!
//! `[workspace.dependencies] reqwest` asked for `rustls-tls` but left
//! reqwest's default features on, and the defaults include `default-tls`
//! (native-tls). That linked OpenSSL through `openssl-sys` on Linux into every
//! crate that depends on sui, engenho included, and it made native-tls the
//! runtime backend: with `default-tls` on, reqwest's `TlsBackend::default()`
//! picks native-tls even when a rustls feature is also enabled.
//!
//! # Why Cargo.lock and not `cargo tree`
//!
//! On darwin native-tls sits on Security.framework, so a host-filtered
//! `cargo tree -i openssl-sys --workspace` printed "nothing to print" while
//! the Linux build linked OpenSSL. `Cargo.lock` is resolved for every target
//! at once, so it records the Linux edge from any host. `cargo test`
//! re-resolves the lockfile before building, so a manifest edit that brings
//! these crates back is caught in the same run.
//!
//! The ban covers every route back in, not just the one that was fixed: a new
//! member asking for reqwest with defaults, a dependency growing a native-tls
//! feature, or a direct OpenSSL dependency.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use serde::Deserialize;

/// One crate that must not appear in the resolve, and what it would bring.
struct Banned {
    name: &'static str,
    why: &'static str,
}

const BANNED: &[Banned] = &[
    Banned {
        name: "openssl-sys",
        why: "links libssl/libcrypto (C) via pkg-config or a vendored build",
    },
    Banned {
        name: "openssl",
        why: "safe wrapper over openssl-sys; present only when OpenSSL is linked",
    },
    Banned {
        name: "native-tls",
        why: "reqwest's `default-tls`: OpenSSL on Linux, Security.framework on darwin",
    },
    Banned {
        name: "hyper-tls",
        why: "hyper connector over native-tls, pulled by reqwest's `default-tls`",
    },
    Banned {
        name: "tokio-native-tls",
        why: "tokio adapter over native-tls, pulled by reqwest's `default-tls`",
    },
];

/// Crates that must stay in the resolve. Without them the ban could pass
/// because TLS (or the lockfile read) went away, not because it is rustls.
const REQUIRED: &[&str] = &["pleme-io-sui", "reqwest", "rustls", "hyper-rustls"];

#[derive(Deserialize)]
struct Lock {
    package: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    #[serde(default)]
    dependencies: Vec<String>,
}

impl Lock {
    fn read() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock");
        let text = std::fs::read_to_string(&path).expect("read the workspace Cargo.lock");
        toml::from_str(&text).expect("parse the workspace Cargo.lock")
    }

    fn contains(&self, name: &str) -> bool {
        self.package.iter().any(|p| p.name == name)
    }

    /// Packages whose dependency list names `name`. A lockfile entry is
    /// `"name"` or `"name version"` when two versions coexist.
    fn dependents_of(&self, name: &str) -> BTreeSet<&str> {
        self.package
            .iter()
            .filter(|p| {
                p.dependencies
                    .iter()
                    .any(|d| d.split(' ').next() == Some(name))
            })
            .map(|p| p.name.as_str())
            .collect()
    }
}

/// A banned crate found in the resolve, with the crates that pull it in.
struct Offence<'a> {
    banned: &'a Banned,
    pulled_by: BTreeSet<&'a str>,
}

impl fmt::Display for Offence<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "  {} ({}); pulled by:", self.banned.name, self.banned.why)?;
        for dependent in &self.pulled_by {
            write!(f, " {dependent}")?;
        }
        Ok(())
    }
}

struct Report<'a>(Vec<Offence<'a>>);

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "C TLS crates re-entered Cargo.lock:")?;
        for offence in &self.0 {
            writeln!(f, "{offence}")?;
        }
        write!(
            f,
            "Keep `default-features = false` on `[workspace.dependencies] reqwest` \
             in Cargo.toml, and find the dependent above that enables `default-tls` \
             or `native-tls`."
        )
    }
}

#[test]
fn no_native_tls_or_openssl_in_the_resolve() {
    let lock = Lock::read();
    let report = Report(
        BANNED
            .iter()
            .filter(|banned| lock.contains(banned.name))
            .map(|banned| Offence {
                banned,
                pulled_by: lock.dependents_of(banned.name),
            })
            .collect(),
    );
    assert!(report.0.is_empty(), "{report}");
}

/// Positive control for the ban above.
#[test]
fn rustls_is_what_remains() {
    let lock = Lock::read();
    for name in REQUIRED {
        assert!(
            lock.contains(name),
            "`{name}` is missing from Cargo.lock; the no-C-TLS check would pass vacuously"
        );
    }
}
