use std::path::Path;
use std::sync::OnceLock;

pub type FlakeSourceFetcher = fn(&str) -> Option<String>;

static FLAKE_SOURCE_FETCHER: OnceLock<FlakeSourceFetcher> = OnceLock::new();

pub fn set_flake_source_fetcher(fetcher: FlakeSourceFetcher) {
    let _ = FLAKE_SOURCE_FETCHER.set(fetcher);
}

fn is_pseudo_url(s: &str) -> bool {
    if s.starts_with("channel:") {
        return true;
    }
    match s.find("://") {
        Some(pos) => matches!(&s[..pos], "http" | "https" | "file" | "channel" | "git" | "s3" | "ssh"),
        None => false,
    }
}

#[must_use]
pub fn parse_nix_path(s: &str) -> Vec<(String, String)> {
    let bytes = s.as_bytes();
    let mut elems = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let mut value_start = i;
        while i < bytes.len() && bytes[i] != b':' {
            i += 1;
            if bytes[i - 1] == b'=' {
                value_start = i;
            }
        }
        if i < bytes.len() {
            let rest = &s[value_start..];
            if is_pseudo_url(rest) || rest.starts_with("flake:") {
                i += 1;
                while i < bytes.len() && bytes[i] != b':' {
                    i += 1;
                }
            }
        }
        elems.push(&s[start..i]);
        i += 1;
    }
    elems
        .into_iter()
        .filter(|e| !e.is_empty())
        .map(|entry| match entry.split_once('=') {
            Some((prefix, path)) => (prefix.to_string(), path.to_string()),
            None => (String::new(), entry.to_string()),
        })
        .collect()
}

fn resolve_target(target: &str) -> Option<String> {
    match target.strip_prefix("flake:") {
        Some(flake_ref) => FLAKE_SOURCE_FETCHER.get().and_then(|fetch| fetch(flake_ref)),
        None => Some(target.to_string()),
    }
}

#[must_use]
pub fn resolve_in(entries: &[(String, String)], name: &str) -> Option<String> {
    for (prefix, target) in entries {
        let candidate = if prefix.is_empty() {
            resolve_target(target).map(|base| format!("{base}/{name}"))
        } else if name == prefix {
            resolve_target(target)
        } else if let Some(rest) = name.strip_prefix(&format!("{prefix}/")) {
            resolve_target(target).map(|base| format!("{base}/{rest}"))
        } else {
            continue;
        };
        if let Some(full) = candidate {
            if Path::new(&full).exists() {
                return Some(full);
            }
        }
    }
    None
}

#[must_use]
pub fn resolve(name: &str) -> Option<String> {
    let nix_path = std::env::var("NIX_PATH").unwrap_or_default();
    resolve_in(&parse_nix_path(&nix_path), name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| ((*a).to_string(), (*b).to_string())).collect()
    }

    #[test]
    fn a_flake_or_url_entry_keeps_its_colons_like_cppnix() {
        assert_eq!(
            parse_nix_path("/home/u/.nix-defexpr/channels:nixpkgs=flake:nixpkgs:/nix/var/channels"),
            pairs(&[("", "/home/u/.nix-defexpr/channels"), ("nixpkgs", "flake:nixpkgs"), ("", "/nix/var/channels")])
        );
        assert_eq!(
            parse_nix_path("a=https://x.org/t.tar.gz:b=/p:channel:nixos-24.05"),
            pairs(&[("a", "https://x.org/t.tar.gz"), ("b", "/p"), ("", "channel:nixos-24.05")])
        );
        assert_eq!(parse_nix_path("a=/x::b=/y:"), pairs(&[("a", "/x"), ("b", "/y")]));
        assert_eq!(parse_nix_path(""), pairs(&[]));
    }

    #[test]
    fn a_flake_entry_resolves_through_the_registered_fetcher() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("lib")).unwrap();
        static ROOT: OnceLock<String> = OnceLock::new();
        ROOT.set(dir.path().to_string_lossy().into_owned()).unwrap();
        set_flake_source_fetcher(|r| (r == "nixpkgs").then(|| ROOT.get().unwrap().clone()));
        let entries = parse_nix_path("nixpkgs=flake:nixpkgs:other=flake:absent");
        assert_eq!(resolve_in(&entries, "nixpkgs"), ROOT.get().cloned());
        assert_eq!(resolve_in(&entries, "nixpkgs/lib"), Some(format!("{}/lib", ROOT.get().unwrap())));
        assert_eq!(resolve_in(&entries, "other"), None);
        assert_eq!(resolve_in(&entries, "nixpkgs/missing"), None);
        // SAFETY: the only test in this crate that reads NIX_PATH.
        unsafe { std::env::set_var("NIX_PATH", "nixpkgs=flake:nixpkgs") };
        assert_eq!(resolve("nixpkgs/lib"), Some(format!("{}/lib", ROOT.get().unwrap())));
        assert_eq!(resolve("absent"), None);
    }
}
