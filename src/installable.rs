//! What a command evaluates, named once: a flake output, a Nix file, or an
//! expression, each with the attribute paths to select. cppnix's rules: a
//! file is resolved against the working directory, a directory means its
//! `default.nix`, and a file whose value is a function is called with `{}`
//! (autoCallFunction); `-A a.b` selects from that result.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    File(PathBuf),
    Expr(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub source: Source,
    pub attr: String,
}

/// `path` as cppnix resolves a file argument: relative to `cwd`, and a
/// directory names its `default.nix`.
#[must_use]
pub fn resolve_file(path: &str, cwd: &Path) -> PathBuf {
    let joined = if Path::new(path).is_absolute() { PathBuf::from(path) } else { cwd.join(path) };
    if joined.is_dir() { joined.join("default.nix") } else { joined }
}

const GET_DERIVATIONS: &str = r#"(let
  isD = x: builtins.isAttrs x && (x.type or null) == "derivation";
  call = x: if builtins.isFunction x then x {} else x;
  named = n: builtins.match "[A-Za-z_][-A-Za-z0-9_+]*" n != null;
  member = y: if isD y then [ y.drvPath ] else if builtins.isAttrs y && (y.recurseForDerivations or false) then go y else [ ];
  go = x0: let x = call x0; in
    if isD x then [ x.drvPath ]
    else if builtins.isAttrs x then builtins.concatMap (n: if named n then member x.${n} else [ ]) (builtins.attrNames x)
    else if builtins.isList x then builtins.concatMap member x
    else throw "expression does not evaluate to a derivation (or a set or list of those)";
in go) "#;

fn quoted(segment: &str) -> String {
    let plain = !segment.is_empty()
        && segment.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '\''))
        && !segment.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '\'');
    if plain {
        segment.to_string()
    } else {
        let escaped = segment.replace('\\', "\\\\").replace('"', "\\\"").replace("${", "\\${");
        ["\"", &escaped, "\""].concat()
    }
}

fn select(attr: &str) -> String {
    attr.split('.').filter(|s| !s.is_empty()).map(|s| [".", &quoted(s)].concat()).collect()
}

impl Selection {
    /// The Nix expression whose value this selection names.
    #[must_use]
    pub fn expression(&self) -> String {
        let root = match &self.source {
            Source::File(path) => {
                let file = path.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
                ["(let v = import \"", &file, "\"; in if builtins.isFunction v then v {} else v)"].concat()
            }
            Source::Expr(expr) => ["(let v = (", expr, "); in if builtins.isFunction v then v {} else v)"].concat(),
        };
        [root.as_str(), &select(&self.attr)].concat()
    }

    /// The expression for this selection's `.drv` path.
    #[must_use]
    pub fn drv_path_expression(&self) -> String {
        ["(", &self.expression(), ").drvPath"].concat()
    }

    /// The `.drv` paths of every derivation this selection names, as a list,
    /// by cppnix's `getDerivations` (libexpr/get-drvs.cc).
    #[must_use]
    pub fn drv_paths_expression(&self) -> String {
        ["(", GET_DERIVATIONS, " (", &self.expression(), "))"].concat()
    }
}

/// The selections a legacy `nix-build` / `nix-instantiate` command line names:
/// each positional file (default `./default.nix`), or each `-E` expression,
/// crossed with each `-A` path (default the whole value).
#[must_use]
pub fn legacy_selections(files: &[String], exprs: bool, attrs: &[String], cwd: &Path) -> Vec<Selection> {
    let sources: Vec<Source> = if exprs {
        files.iter().cloned().map(Source::Expr).collect()
    } else if files.is_empty() {
        vec![Source::File(resolve_file("./default.nix", cwd))]
    } else {
        files.iter().map(|f| Source::File(resolve_file(f, cwd))).collect()
    };
    let attrs: Vec<String> = if attrs.is_empty() { vec![String::new()] } else { attrs.to_vec() };
    sources
        .iter()
        .flat_map(|source| attrs.iter().map(move |attr| Selection { source: source.clone(), attr: attr.clone() }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_file_resolves_against_the_working_directory_and_a_directory_means_default_nix() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("pkg")).unwrap();
        assert_eq!(resolve_file("./x.nix", tmp.path()), tmp.path().join("x.nix"));
        assert_eq!(resolve_file("pkg", tmp.path()), tmp.path().join("pkg/default.nix"));
        assert_eq!(resolve_file("/abs/y.nix", tmp.path()), PathBuf::from("/abs/y.nix"));
    }

    #[test]
    fn no_file_means_default_nix_and_each_attr_selects_from_each_source() {
        let cwd = Path::new("/w");
        let s = legacy_selections(&[], false, &["a".into(), "b.c".into()], cwd);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].source, Source::File(PathBuf::from("/w/./default.nix")));
        assert_eq!(s[1].attr, "b.c");
        let e = legacy_selections(&["{ x = 1; }".into()], true, &[], cwd);
        assert_eq!(e, vec![Selection { source: Source::Expr("{ x = 1; }".into()), attr: String::new() }]);
    }

    #[test]
    fn derivations_are_found_as_cppnix_get_derivations_finds_them() {
        sui_store::drv_write::set_process_mode(sui_store::drv_write::DrvWriteMode::ReadOnly);
        let set = r#"let d = n: derivation { name = n; system = "x"; builder = "/bin/sh"; };
          in { b = d "b"; a = d "a"; same = d "a"; inner = { recurseForDerivations = true; c = d "c"; };
               hidden = { e = d "e"; }; "9bad" = d "bad"; n = 3; }"#;
        let s = Selection { source: Source::Expr(set.into()), attr: String::new() };
        let names = ["builtins.toJSON (map (p: builtins.substring 44 100 p) ", &s.drv_paths_expression(), ")"].concat();
        let v = sui_eval::eval(&names).unwrap();
        assert_eq!(v.to_string(), r#""[\"a.drv\",\"b.drv\",\"c.drv\",\"a.drv\"]""#.replace("\\\"", "\\\""));
    }

    #[test]
    fn an_attr_path_quotes_what_is_not_an_identifier() {
        let s = Selection { source: Source::Expr("x".into()), attr: "pkgs.foo-bar.1x.a b".into() };
        assert!(s.expression().ends_with(".pkgs.foo-bar.\"1x\".\"a b\""), "{}", s.expression());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreInstallable {
    Derivation(String),
    Path(String),
}

impl StoreInstallable {
    #[must_use]
    pub fn parse(installable: &str) -> Option<Self> {
        let (base, outputs) = match installable.split_once('^') {
            Some((base, outputs)) => (base, Some(outputs)),
            None => (installable, None),
        };
        let rest = base.strip_prefix("/nix/store/")?;
        let name = rest.split('/').next().filter(|n| !n.is_empty())?;
        let root = format!("/nix/store/{name}");
        match (name.ends_with(".drv"), outputs) {
            (true, Some(_)) => Some(Self::Derivation(root)),
            (true, None) => None,
            (false, None) => Some(Self::Path(root)),
            (false, Some(_)) => None,
        }
    }

    #[must_use]
    pub fn follow_links(installable: &str) -> Option<Self> {
        if !installable.contains('/') || installable.contains('#') {
            return None;
        }
        let real = std::fs::canonicalize(installable).ok()?;
        let real = real.to_str()?;
        real.starts_with("/nix/store/").then(|| Self::parse(real)).flatten()
    }
}

#[cfg(test)]
mod store_installable_tests {
    use super::StoreInstallable;

    #[test]
    fn a_store_path_or_a_derivation_with_outputs_is_built_as_cppnix_reads_it() {
        let p = "/nix/store/7hdmnzw8bzchrwvqyhahdrvrijmf414f-nix-2.31.5";
        assert_eq!(StoreInstallable::parse(p), Some(StoreInstallable::Path(p.into())));
        assert_eq!(StoreInstallable::parse(&format!("{p}/bin/nix")), Some(StoreInstallable::Path(p.into())));
        let d = "/nix/store/5i38p4am32wcmf0acdizd7880md2bzd5-countme-3.0.1.tar.gz.drv";
        assert_eq!(StoreInstallable::parse(&format!("{d}^*")), Some(StoreInstallable::Derivation(d.into())));
        assert_eq!(StoreInstallable::parse(&format!("{d}^out,dev")), Some(StoreInstallable::Derivation(d.into())));
        assert_eq!(StoreInstallable::parse(d), None);
        assert_eq!(StoreInstallable::parse(".#default"), None);
        assert_eq!(StoreInstallable::parse("nixpkgs#hello"), None);
        assert_eq!(StoreInstallable::parse("/nix/store/"), None);
        assert_eq!(StoreInstallable::parse(&format!("{p}^out")), None);
    }

    #[test]
    fn a_link_into_the_store_is_the_store_path_it_reaches() {
        let target = std::fs::read_dir("/nix/store")
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.is_dir() && !p.to_string_lossy().ends_with(".drv"))
            .expect("a store directory");
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("profile");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let want = StoreInstallable::Path(target.to_string_lossy().into_owned());
        assert_eq!(StoreInstallable::follow_links(link.to_str().unwrap()), Some(want));
        assert_eq!(StoreInstallable::follow_links(dir.path().to_str().unwrap()), None);
        assert_eq!(StoreInstallable::follow_links("."), None);
        assert_eq!(StoreInstallable::follow_links("./x#y"), None);
    }
}
