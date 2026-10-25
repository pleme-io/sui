//! Canonical absolute paths: cppnix's `CanonPath` (libutil canon-path.cc, `canonPathInner`).

use std::fmt;

/// An absolute path with no `.`, no `..`, no empty or repeated separators and
/// no trailing slash; `/` is the root. The only constructor canonicalizes.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonPath(Box<str>);

impl CanonPath {
    /// Canonicalizes `raw` as cppnix's `CanonPath(raw)`: a relative `raw` is taken from `/`.
    #[must_use]
    pub fn new(raw: &str) -> Self {
        Self(canonicalize(raw).into_boxed_str())
    }

    /// The canonical path text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn canonicalize(raw: &str) -> String {
    let mut result = String::with_capacity(raw.len() + 1);
    for comp in raw.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if let Some(cut) = result.rfind('/') {
                    result.truncate(cut);
                }
            }
            c => {
                result.push('/');
                result.push_str(c);
            }
        }
    }
    if result.is_empty() {
        result.push('/');
    }
    result
}

impl std::ops::Deref for CanonPath {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for CanonPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<std::path::Path> for CanonPath {
    fn as_ref(&self) -> &std::path::Path {
        std::path::Path::new(&*self.0)
    }
}

impl fmt::Display for CanonPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for CanonPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl PartialEq<str> for CanonPath {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for CanonPath {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl PartialEq<String> for CanonPath {
    fn eq(&self, other: &String) -> bool {
        *self.0 == **other
    }
}

impl From<&str> for CanonPath {
    fn from(raw: &str) -> Self {
        Self::new(raw)
    }
}

impl From<String> for CanonPath {
    fn from(raw: String) -> Self {
        Self::new(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::CanonPath;

    #[test]
    fn matches_cppnix_canon_path() {
        for (raw, canon) in [
            ("/", "/"),
            ("", "/"),
            ("/nix/..", "/"),
            ("/nix/store/", "/nix/store"),
            ("//nix//store", "/nix/store"),
            ("/nix/./store", "/nix/store"),
            ("/..", "/"),
            ("/a/b/../../..", "/"),
            ("a/b", "/a/b"),
            ("/a/.../b", "/a/.../b"),
        ] {
            assert_eq!(CanonPath::new(raw).as_str(), canon, "{raw:?}");
        }
    }
}
