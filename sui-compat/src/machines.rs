use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scheme {
    Ssh,
    SshNg,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreUri {
    pub raw: String,
    pub scheme: Scheme,
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub params: BTreeMap<String, String>,
}

impl StoreUri {
    pub fn parse(uri: &str) -> Result<Self, MachinesError> {
        let completed = if uri.contains("://")
            || uri.contains('/')
            || ["auto", "daemon", "local"].contains(&uri)
            || ["auto?", "daemon?", "local?", "?"].iter().any(|p| uri.starts_with(p))
        {
            uri.to_string()
        } else {
            format!("ssh://{uri}")
        };
        let Some((scheme, rest)) = completed.split_once("://") else {
            return Ok(Self {
                raw: completed.clone(),
                scheme: Scheme::Other(completed),
                user: None,
                host: String::new(),
                port: None,
                params: BTreeMap::new(),
            });
        };
        let (authority, query) = rest.split_once('?').map_or((rest, ""), |(a, q)| (a, q));
        let params = query
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| kv.split_once('=').map_or((percent_decode(kv), String::new()), |(k, v)| (percent_decode(k), percent_decode(v))))
            .collect();
        let (user, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u.to_string()), h),
            None => (None, authority),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') || h.starts_with('[') => {
                let port = p.parse::<u16>().map_err(|_| MachinesError::BadPort(uri.to_string()))?;
                (h.to_string(), Some(port))
            }
            _ => (hostport.to_string(), None),
        };
        let scheme = match scheme {
            "ssh" => Scheme::Ssh,
            "ssh-ng" => Scheme::SshNg,
            other => Scheme::Other(other.to_string()),
        };
        Ok(Self { raw: completed, scheme, user, host, port, params })
    }

    #[must_use]
    pub fn host_and_user(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Machine {
    pub store_uri: StoreUri,
    pub system_types: BTreeSet<String>,
    pub ssh_key: String,
    pub max_jobs: u32,
    pub speed_factor: f32,
    pub supported_features: BTreeSet<String>,
    pub mandatory_features: BTreeSet<String>,
    pub ssh_public_host_key: String,
}

impl Machine {
    #[must_use]
    pub fn system_supported(&self, system: &str) -> bool {
        system == "builtin" || self.system_types.contains(system)
    }

    #[must_use]
    pub fn all_supported(&self, features: &BTreeSet<String>) -> bool {
        features.iter().all(|f| self.supported_features.contains(f) || self.mandatory_features.contains(f))
    }

    #[must_use]
    pub fn mandatory_met(&self, features: &BTreeSet<String>) -> bool {
        self.mandatory_features.iter().all(|f| features.contains(f))
    }

    #[must_use]
    pub fn accepts(&self, system: &str, features: &BTreeSet<String>) -> bool {
        self.system_supported(system) && self.all_supported(features) && self.mandatory_met(features)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MachinesError {
    #[error("bad machine specification: store URL was not found at the first column of a row: '{0}'")]
    MissingUri(String),
    #[error("bad machine specification: failed to convert column #{column} in a row: '{line}' to '{kind}'")]
    BadField { column: usize, line: String, kind: &'static str },
    #[error("speed factor must be >= 0")]
    NegativeSpeed,
    #[error("while parsing machine specification at a column #7 in a row: '{0}': invalid base64")]
    BadHostKey(String),
    #[error("bad port in store URI '{0}'")]
    BadPort(String),
    #[error("cannot read machines file '{path}': {message}")]
    Unreadable { path: String, message: String },
}

pub fn parse_config(
    default_systems: &BTreeSet<String>,
    builders: &str,
    read: &dyn Fn(&Path) -> std::io::Result<String>,
) -> Result<Vec<Machine>, MachinesError> {
    expand_builder_lines(builders, read)?
        .iter()
        .map(|line| parse_builder_line(default_systems, line))
        .collect()
}

#[must_use]
pub fn default_builders(nix_conf_dir: &Path) -> String {
    format!("@{}", nix_conf_dir.join("machines").display())
}

fn expand_builder_lines(
    builders: &str,
    read: &dyn Fn(&Path) -> std::io::Result<String>,
) -> Result<Vec<String>, MachinesError> {
    let mut result = Vec::new();
    for line in builders.split('\n').filter(|l| !l.is_empty()) {
        let line = line.split('#').next().unwrap_or_default();
        for entry in line.split(';').filter(|e| !e.is_empty()) {
            let entry = entry.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'));
            if entry.is_empty() {
                continue;
            }
            if let Some(path) = entry.strip_prefix('@') {
                let path = path.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'));
                match read(Path::new(path)) {
                    Ok(text) => result.extend(expand_builder_lines(&text, read)?),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(MachinesError::Unreadable { path: path.to_string(), message: e.to_string() });
                    }
                }
            } else {
                result.push(entry.to_string());
            }
        }
    }
    Ok(result)
}

fn parse_builder_line(default_systems: &BTreeSet<String>, line: &str) -> Result<Machine, MachinesError> {
    let tokens: Vec<&str> = line.split([' ', '\t', '\n', '\r']).filter(|t| !t.is_empty()).collect();
    let is_set = |i: usize| tokens.get(i).is_some_and(|t| !t.is_empty() && *t != "-");
    let set_of = |i: usize| -> BTreeSet<String> { tokens[i].split(',').filter(|s| !s.is_empty()).map(String::from).collect() };
    if !is_set(0) {
        return Err(MachinesError::MissingUri(line.to_string()));
    }
    let max_jobs = if is_set(3) {
        tokens[3].parse::<u32>().map_err(|_| MachinesError::BadField { column: 3, line: line.to_string(), kind: "unsigned int" })?
    } else {
        1
    };
    let speed_factor = if is_set(4) {
        tokens[4].parse::<f32>().map_err(|_| MachinesError::BadField { column: 4, line: line.to_string(), kind: "float" })?
    } else {
        1.0
    };
    if speed_factor < 0.0 {
        return Err(MachinesError::NegativeSpeed);
    }
    let ssh_public_host_key = if is_set(7) {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(tokens[7])
            .map_err(|_| MachinesError::BadHostKey(line.to_string()))?;
        tokens[7].to_string()
    } else {
        String::new()
    };
    Ok(Machine {
        store_uri: StoreUri::parse(tokens[0])?,
        system_types: if is_set(1) { set_of(1) } else { default_systems.clone() },
        ssh_key: if is_set(2) { tokens[2].to_string() } else { String::new() },
        max_jobs,
        speed_factor: if speed_factor == 0.0 { 1.0 } else { speed_factor },
        supported_features: if is_set(5) { set_of(5) } else { BTreeSet::new() },
        mandatory_features: if is_set(6) { set_of(6) } else { BTreeSet::new() },
        ssh_public_host_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn systems(s: &[&str]) -> BTreeSet<String> {
        s.iter().map(|x| (*x).to_string()).collect()
    }

    fn no_files(_: &Path) -> std::io::Result<String> {
        Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    }

    fn parse(text: &str) -> Vec<Machine> {
        parse_config(&systems(&["aarch64-darwin"]), text, &no_files).unwrap()
    }

    #[test]
    fn a_bare_host_is_an_ssh_store_with_cppnix_defaults() {
        let m = &parse("nix@scratchy.labs.cs.uu.nl")[0];
        assert_eq!(m.store_uri.raw, "ssh://nix@scratchy.labs.cs.uu.nl");
        assert_eq!(m.store_uri.scheme, Scheme::Ssh);
        assert_eq!(m.store_uri.user.as_deref(), Some("nix"));
        assert_eq!(m.store_uri.host, "scratchy.labs.cs.uu.nl");
        assert_eq!(m.system_types, systems(&["aarch64-darwin"]));
        assert_eq!(m.max_jobs, 1);
        assert!((m.speed_factor - 1.0).abs() < f32::EPSILON);
        assert!(m.supported_features.is_empty() && m.mandatory_features.is_empty());
        assert_eq!(m.ssh_key, "");
    }

    #[test]
    fn every_column_reads_like_the_cppnix_manual_example() {
        let text = "ssh://root@plo x86_64-linux,i686-linux /var/root/.ssh/k 4 8 nixos-test,benchmark,big-parallel,kvm kvm c3NoLWVkMjU1MTkgQUFBQQ==";
        let m = &parse(text)[0];
        assert_eq!(m.system_types, systems(&["i686-linux", "x86_64-linux"]));
        assert_eq!(m.ssh_key, "/var/root/.ssh/k");
        assert_eq!(m.max_jobs, 4);
        assert!((m.speed_factor - 8.0).abs() < f32::EPSILON);
        assert_eq!(m.supported_features, systems(&["benchmark", "big-parallel", "kvm", "nixos-test"]));
        assert_eq!(m.mandatory_features, systems(&["kvm"]));
        assert_eq!(m.ssh_public_host_key, "c3NoLWVkMjU1MTkgQUFBQQ==");
    }

    #[test]
    fn a_dash_and_a_missing_column_both_take_the_default() {
        let m = &parse("ssh-ng://cid - - - 0")[0];
        assert_eq!(m.store_uri.scheme, Scheme::SshNg);
        assert_eq!(m.system_types, systems(&["aarch64-darwin"]));
        assert!((m.speed_factor - 1.0).abs() < f32::EPSILON, "a zero speed factor reads as 1");
    }

    #[test]
    fn semicolons_newlines_comments_and_files_expand_in_order() {
        let read = |p: &Path| -> std::io::Result<String> {
            match p.to_str() {
                Some("/etc/nix/machines") => Ok("b x86_64-linux # rio\n\n@/etc/nix/more\n".into()),
                Some("/etc/nix/more") => Ok("c".into()),
                _ => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            }
        };
        let ms = parse_config(&systems(&["aarch64-darwin"]), "a ; @/etc/nix/machines ; @/missing\n# d\n", &read).unwrap();
        let hosts: Vec<&str> = ms.iter().map(|m| m.store_uri.host.as_str()).collect();
        assert_eq!(hosts, ["a", "b", "c"]);
        assert_eq!(ms[1].system_types, systems(&["x86_64-linux"]));
    }

    #[test]
    fn an_empty_builders_setting_has_no_machines() {
        assert!(parse("").is_empty());
        assert!(parse("  ;  ").is_empty());
    }

    #[test]
    fn bad_columns_are_refused_with_cppnix_messages() {
        let d = systems(&["x"]);
        let e = parse_config(&d, "host x86_64-linux - four", &no_files).unwrap_err();
        assert_eq!(e.to_string(), "bad machine specification: failed to convert column #3 in a row: 'host x86_64-linux - four' to 'unsigned int'");
        assert!(matches!(parse_config(&d, "host - - 1 fast", &no_files), Err(MachinesError::BadField { column: 4, .. })));
        assert!(matches!(parse_config(&d, "host - - 1 -2", &no_files), Err(MachinesError::NegativeSpeed)));
        assert!(matches!(parse_config(&d, "host - - - - - - !!!", &no_files), Err(MachinesError::BadHostKey(_))));
        assert!(matches!(parse_config(&d, "- x86_64-linux", &no_files), Err(MachinesError::MissingUri(_))));
    }

    #[test]
    fn an_unreadable_file_other_than_missing_is_an_error() {
        let read = |_: &Path| -> std::io::Result<String> { Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)) };
        assert!(matches!(parse_config(&systems(&[]), "@/x", &read), Err(MachinesError::Unreadable { .. })));
    }

    #[test]
    fn selection_rules_match_cppnix_system_and_features() {
        let m = &parse("h x86_64-linux - 1 1 big-parallel kvm")[0];
        let f = |s: &[&str]| systems(s);
        assert!(m.accepts("x86_64-linux", &f(&["kvm"])));
        assert!(m.accepts("x86_64-linux", &f(&["kvm", "big-parallel"])));
        assert!(!m.accepts("x86_64-linux", &f(&[])), "a mandatory feature must be required");
        assert!(!m.accepts("x86_64-linux", &f(&["kvm", "benchmark"])), "every required feature must be supported");
        assert!(!m.accepts("aarch64-linux", &f(&["kvm"])));
        assert!(m.accepts("builtin", &f(&["kvm"])));
    }

    #[test]
    fn store_uri_carries_port_and_params() {
        let u = StoreUri::parse("ssh-ng://me@cid:2222?remote-program=/x/nix-daemon&ssh-key=/k").unwrap();
        assert_eq!(u.port, Some(2222));
        assert_eq!(u.host_and_user(), "me@cid");
        assert_eq!(u.params.get("remote-program").map(String::as_str), Some("/x/nix-daemon"));
        assert_eq!(StoreUri::parse("daemon").unwrap().scheme, Scheme::Other("daemon".into()));
    }
}
