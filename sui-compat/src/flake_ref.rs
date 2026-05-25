//! Flake reference parser for CLI-style references like `.#cid`.
//!
//! Parses the `<path>#<attribute>` format used by `nix build`, `nix eval`,
//! and `sui system rebuild --flake`.

use std::path::PathBuf;

/// A parsed flake reference like `.#cid` or `path/to/flake#hostname`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlakeRef {
    /// The directory containing the flake.
    pub flake_dir: PathBuf,
    /// The attribute path after the `#`.
    pub attribute: String,
}

impl FlakeRef {
    /// Parse a CLI-style flake reference.
    ///
    /// # Format
    ///
    /// `<path>#<attribute>` where `<path>` is a filesystem path and
    /// `<attribute>` is a dot-separated Nix attribute path.
    ///
    /// # Examples
    ///
    /// - `.#cid` — current directory, attribute `cid`
    /// - `/path/to/nix#cid` — absolute path, attribute `cid`
    /// - `relative/path#attr` — relative path, attribute `attr`
    /// - `.#` — current directory, empty attribute (allowed)
    ///
    /// # Errors
    ///
    /// Returns [`FlakeRefError::MissingAttribute`] if the input does not
    /// contain a `#` separator, and [`FlakeRefError::InvalidPath`] if the
    /// current directory cannot be resolved (only when path is `.` or empty).
    pub fn parse(input: &str) -> Result<Self, FlakeRefError> {
        if let Some((path_part, attr)) = input.split_once('#') {
            let dir = if path_part == "." || path_part.is_empty() {
                std::env::current_dir()
                    .map_err(|e| FlakeRefError::InvalidPath(e.to_string()))?
            } else {
                // Strip the explicit `path:` scheme — both forms are
                // valid CLI input (`nix build path:/dir#attr` and
                // `nix build /dir#attr` mean the same thing).  Without
                // this, `PathBuf::from("path:/dir")` produces a literal
                // `path:/dir/flake.nix` join target that doesn't exist.
                let raw = path_part.strip_prefix("path:").unwrap_or(path_part);
                PathBuf::from(raw)
            };
            Ok(Self {
                flake_dir: dir,
                attribute: attr.to_string(),
            })
        } else {
            Err(FlakeRefError::MissingAttribute(input.to_string()))
        }
    }
}

/// Errors from parsing a flake reference.
#[derive(Debug, thiserror::Error)]
pub enum FlakeRefError {
    /// The input string did not contain a `#` separator.
    #[error("flake reference missing '#attribute': {0}")]
    MissingAttribute(String),
    /// The path component could not be resolved.
    #[error("invalid flake path: {0}")]
    InvalidPath(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dot_hash_cid() {
        let fr = FlakeRef::parse(".#cid").unwrap();
        // flake_dir should be the current working directory
        assert_eq!(fr.flake_dir, std::env::current_dir().unwrap());
        assert_eq!(fr.attribute, "cid");
    }

    #[test]
    fn parse_absolute_path() {
        let fr = FlakeRef::parse("/absolute/path#attr").unwrap();
        assert_eq!(fr.flake_dir, PathBuf::from("/absolute/path"));
        assert_eq!(fr.attribute, "attr");
    }

    #[test]
    fn parse_relative_path() {
        let fr = FlakeRef::parse("relative/path#attr").unwrap();
        assert_eq!(fr.flake_dir, PathBuf::from("relative/path"));
        assert_eq!(fr.attribute, "attr");
    }

    #[test]
    fn parse_missing_hash_returns_error() {
        let err = FlakeRef::parse("no-hash-here").unwrap_err();
        assert!(matches!(err, FlakeRefError::MissingAttribute(_)));
        assert!(err.to_string().contains("no-hash-here"));
    }

    #[test]
    fn parse_empty_attribute_allowed() {
        let fr = FlakeRef::parse(".#").unwrap();
        assert_eq!(fr.attribute, "");
    }

    #[test]
    fn parse_empty_path_uses_cwd() {
        let fr = FlakeRef::parse("#attr").unwrap();
        assert_eq!(fr.flake_dir, std::env::current_dir().unwrap());
        assert_eq!(fr.attribute, "attr");
    }

    #[test]
    fn parse_dotted_attribute() {
        let fr = FlakeRef::parse("/nix#darwinConfigurations.cid.system").unwrap();
        assert_eq!(fr.flake_dir, PathBuf::from("/nix"));
        assert_eq!(fr.attribute, "darwinConfigurations.cid.system");
    }

    #[test]
    fn parse_strips_path_scheme() {
        let fr = FlakeRef::parse("path:/etc/nixos#nixosConfigurations.rio").unwrap();
        assert_eq!(fr.flake_dir, PathBuf::from("/etc/nixos"));
        assert_eq!(fr.attribute, "nixosConfigurations.rio");
    }

    #[test]
    fn parse_strips_path_scheme_relative() {
        let fr = FlakeRef::parse("path:./config#attr").unwrap();
        assert_eq!(fr.flake_dir, PathBuf::from("./config"));
        assert_eq!(fr.attribute, "attr");
    }

    #[test]
    fn parse_multiple_hashes_splits_on_first() {
        let fr = FlakeRef::parse("/path#attr#extra").unwrap();
        assert_eq!(fr.flake_dir, PathBuf::from("/path"));
        assert_eq!(fr.attribute, "attr#extra");
    }

    #[test]
    fn error_display_missing_attribute() {
        let err = FlakeRefError::MissingAttribute("foo".into());
        assert!(err.to_string().contains("missing '#attribute'"));
        assert!(err.to_string().contains("foo"));
    }

    #[test]
    fn error_display_invalid_path() {
        let err = FlakeRefError::InvalidPath("bad path".into());
        assert!(err.to_string().contains("invalid flake path"));
        assert!(err.to_string().contains("bad path"));
    }
}
