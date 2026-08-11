//! Flake reference parser for CLI-style references like `.#cid`.
//!
//! Parses the `<path>#<attribute>` format used by `nix build`, `nix eval`,
//! and `sui system rebuild --flake`.

use std::path::PathBuf;

/// WHERE a flake reference's source lives.
///
/// ── ★ THE ONE THING THAT KEPT sui OUT OF THE FLEET RECONCILER ──────────
/// This used to be a bare `PathBuf`, i.e. every ref was assumed to name a
/// directory already on disk. sentinela's whole safety model is a
/// REV-PINNED REMOTE ref — it resolves branch HEAD over the git protocol and
/// then builds `github:owner/repo/<rev>#host` — so `RebuildTool::Sui` was
/// typed as `FlakeRefSyntax::LocalPathOnly` and `preflight` refused the
/// pairing outright rather than fail every tick behind a green heartbeat.
///
/// The fetcher was never the gap. `sui_eval::fetcher` has fetched github
/// tarballs for locked flake INPUTS since long before this; only the
/// TOP-LEVEL ref was path-only. So this is a routing fix, not a new
/// capability: [`Self::GitHub`] carries exactly the [`LockedInput`] that
/// fetcher already consumes.
///
/// Resolution deliberately does NOT happen here. This crate has no fetcher
/// and must not grow a dependency on one; the consumer that already depends
/// on both (`sui-orchestrate`) turns a `GitHub` source into a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlakeSource {
    /// A directory on disk, already present.
    Path(PathBuf),
    /// A GitHub repo at an exact rev, to be fetched before evaluation.
    GitHub {
        /// Repository owner.
        owner: String,
        /// Repository name.
        repo: String,
        /// The exact commit. Required — see [`FlakeRefError::UnpinnedRemote`].
        rev: String,
    },
}

/// A parsed flake reference like `.#cid` or
/// `github:pleme-io/nix/<rev>#cid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlakeRef {
    /// Where the flake comes from.
    pub source: FlakeSource,
    /// The attribute path after the `#`.
    pub attribute: String,
}

impl FlakeSource {
    /// The [`LockedInput`] describing a [`Self::GitHub`] source, ready for
    /// `sui_eval::fetcher::InputFetcher::fetch`. `None` for a path source, which
    /// needs no fetching.
    ///
    /// Built here rather than at the call site so the mapping from parsed
    /// ref to fetcher input exists exactly once.
    #[must_use]
    pub fn locked_input(&self) -> Option<crate::flake::LockedInput> {
        match self {
            Self::Path(_) => None,
            Self::GitHub { owner, repo, rev } => Some(crate::flake::LockedInput {
                source_type: "github".to_owned(),
                owner: Some(owner.clone()),
                repo: Some(repo.clone()),
                rev: Some(rev.clone()),
                ..Default::default()
            }),
        }
    }
}

impl FlakeRef {
    /// The on-disk directory for a [`FlakeSource::Path`] ref, or `None` for
    /// a remote one that must be fetched first.
    ///
    /// Deliberately an `Option` rather than a fallible "just give me a path":
    /// a remote ref has no directory until something fetches it, and the type
    /// is what stops a caller evaluating a path that was never materialised.
    #[must_use]
    pub fn local_dir(&self) -> Option<&std::path::Path> {
        match &self.source {
            FlakeSource::Path(p) => Some(p.as_path()),
            FlakeSource::GitHub { .. } => None,
        }
    }

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
            if let Some(spec) = path_part.strip_prefix("github:") {
                // `owner/repo/rev`. A trailing `?dir=` / `?ref=` query is not
                // accepted rather than silently ignored: dropping `dir=` would
                // evaluate the WRONG flake and report success.
                if spec.contains('?') {
                    return Err(FlakeRefError::UnsupportedRemote(path_part.to_owned()));
                }
                let parts: Vec<&str> = spec.split('/').collect();
                return match parts.as_slice() {
                    [owner, repo, rev] if !owner.is_empty() && !repo.is_empty() && !rev.is_empty() => {
                        Ok(Self {
                            source: FlakeSource::GitHub {
                                owner: (*owner).to_owned(),
                                repo: (*repo).to_owned(),
                                rev: (*rev).to_owned(),
                            },
                            attribute: attr.to_owned(),
                        })
                    }
                    // A branch or bare repo needs a registry/API round trip to
                    // pin, and an unpinned build is not what any caller here
                    // wants. Refused loudly instead of guessed.
                    [_, _] => Err(FlakeRefError::UnpinnedRemote(path_part.to_owned())),
                    _ => Err(FlakeRefError::UnsupportedRemote(path_part.to_owned())),
                };
            }
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
                source: FlakeSource::Path(dir),
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
    /// A `github:` ref naming a branch or bare repo rather than a commit.
    #[error(
        "flake reference is not pinned to a commit: {0} \
         (expected github:owner/repo/<rev>)"
    )]
    UnpinnedRemote(String),
    /// A `github:` ref whose shape this parser does not handle.
    #[error("unsupported remote flake reference: {0}")]
    UnsupportedRemote(String),
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn parses_the_rev_pinned_github_ref_the_fleet_reconciler_uses() {
        // The exact shape sentinela builds: resolve branch HEAD over git, then
        // build `<flake_url>/<rev>#<hostname>`. This ref returning
        // InvalidPath is why RebuildTool::Sui was unreachable.
        let fr = FlakeRef::parse(
            "github:pleme-io/nix/97cbcf317c3bf64407528473e48a01db37142f35#cid",
        )
        .expect("a rev-pinned github ref must parse");

        assert_eq!(fr.attribute, "cid");
        assert_eq!(
            fr.source,
            FlakeSource::GitHub {
                owner: "pleme-io".to_owned(),
                repo: "nix".to_owned(),
                rev: "97cbcf317c3bf64407528473e48a01db37142f35".to_owned(),
            }
        );
        assert!(
            fr.local_dir().is_none(),
            "a remote ref has no directory until it is fetched"
        );
    }

    #[test]
    fn a_github_ref_carries_the_locked_input_the_fetcher_already_consumes() {
        // The routing claim, asserted: this is the same struct
        // `sui_eval::fetcher::InputFetcher::fetch` takes for a locked flake INPUT,
        // so nothing new has to learn how to fetch.
        let fr = FlakeRef::parse("github:pleme-io/nix/deadbeef#cid").unwrap();
        let li = fr.source.locked_input().expect("github source must yield one");

        assert_eq!(li.source_type, "github");
        assert_eq!(li.owner.as_deref(), Some("pleme-io"));
        assert_eq!(li.repo.as_deref(), Some("nix"));
        assert_eq!(li.rev.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn a_path_source_yields_no_locked_input() {
        let fr = FlakeRef::parse("/some/dir#cid").unwrap();
        assert!(fr.source.locked_input().is_none(), "a path needs no fetch");
    }

    #[test]
    fn an_unpinned_github_ref_is_refused_rather_than_guessed() {
        // `github:owner/repo#attr` names a branch, which needs a registry or
        // API round trip to pin. Building an unpinned ref would silently
        // deploy whatever main happened to be — the precise thing sentinela's
        // rev-pinning exists to prevent.
        let err = FlakeRef::parse("github:pleme-io/nix#cid")
            .expect_err("a branch ref must not resolve silently");
        assert!(
            matches!(err, FlakeRefError::UnpinnedRemote(_)),
            "must name the reason: {err}"
        );
    }

    #[test]
    fn a_query_bearing_github_ref_is_refused_not_ignored() {
        // `?dir=sub` selects a DIFFERENT flake. Ignoring it would evaluate the
        // wrong one and report success.
        let err = FlakeRef::parse("github:pleme-io/nix/deadbeef?dir=sub#cid")
            .expect_err("an unsupported query must not be dropped");
        assert!(matches!(err, FlakeRefError::UnsupportedRemote(_)), "{err}");
    }

    #[test]
    fn parse_dot_hash_cid() {
        let fr = FlakeRef::parse(".#cid").unwrap();
        // flake_dir should be the current working directory
        assert_eq!(fr.local_dir().unwrap().to_path_buf(), std::env::current_dir().unwrap());
        assert_eq!(fr.attribute, "cid");
    }

    #[test]
    fn parse_absolute_path() {
        let fr = FlakeRef::parse("/absolute/path#attr").unwrap();
        assert_eq!(fr.local_dir().unwrap().to_path_buf(), PathBuf::from("/absolute/path"));
        assert_eq!(fr.attribute, "attr");
    }

    #[test]
    fn parse_relative_path() {
        let fr = FlakeRef::parse("relative/path#attr").unwrap();
        assert_eq!(fr.local_dir().unwrap().to_path_buf(), PathBuf::from("relative/path"));
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
        assert_eq!(fr.local_dir().unwrap().to_path_buf(), std::env::current_dir().unwrap());
        assert_eq!(fr.attribute, "attr");
    }

    #[test]
    fn parse_dotted_attribute() {
        let fr = FlakeRef::parse("/nix#darwinConfigurations.cid.system").unwrap();
        assert_eq!(fr.local_dir().unwrap().to_path_buf(), PathBuf::from("/nix"));
        assert_eq!(fr.attribute, "darwinConfigurations.cid.system");
    }

    #[test]
    fn parse_strips_path_scheme() {
        let fr = FlakeRef::parse("path:/etc/nixos#nixosConfigurations.rio").unwrap();
        assert_eq!(fr.local_dir().unwrap().to_path_buf(), PathBuf::from("/etc/nixos"));
        assert_eq!(fr.attribute, "nixosConfigurations.rio");
    }

    #[test]
    fn parse_strips_path_scheme_relative() {
        let fr = FlakeRef::parse("path:./config#attr").unwrap();
        assert_eq!(fr.local_dir().unwrap().to_path_buf(), PathBuf::from("./config"));
        assert_eq!(fr.attribute, "attr");
    }

    #[test]
    fn parse_multiple_hashes_splits_on_first() {
        let fr = FlakeRef::parse("/path#attr#extra").unwrap();
        assert_eq!(fr.local_dir().unwrap().to_path_buf(), PathBuf::from("/path"));
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
