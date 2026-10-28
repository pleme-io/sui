use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use clap::CommandFactory;
use serde::{Deserialize, Serialize};

use crate::{Cli, cli_contract, legacy};

const SNAPSHOT_FILE: &str = "parity/cppnix-2.31.5.json";
const PENDING_FILE: &str = "parity/pending.txt";
const SNAPSHOT: &str = include_str!("../parity/cppnix-2.31.5.json");
const PENDING: &str = include_str!("../parity/pending.txt");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "category", rename_all = "kebab-case")]
pub enum Row {
    NixCommand { path: Vec<String> },
    NixFlag { path: Vec<String>, flag: String, short: Option<String>, arity: u64 },
    NixGlobalFlag { flag: String, short: Option<String>, arity: u64 },
    Setting { name: String },
    LegacyCommand { bin: String },
    LegacyOp { bin: String, op: String },
    LegacyFlag { bin: String, op: Option<String>, flag: String },
    LegacyCommonFlag { bin: String, flag: String, arity: u64 },
    StoreScheme { store: String, scheme: String },
    StoreSetting { store: String, scheme: String, name: String },
    Fetcher { name: String },
    XpFeature { name: String },
    Builtin { name: String },
    WorkerOp { name: String, code: u64 },
    ServeCommand { name: String, code: u64 },
    MachinesField { name: String },
    EnvVar { name: String },
}

impl Row {
    pub fn category(&self) -> &'static str {
        match self {
            Self::NixCommand { .. } => "nix-command",
            Self::NixFlag { .. } => "nix-flag",
            Self::NixGlobalFlag { .. } => "nix-global-flag",
            Self::Setting { .. } => "setting",
            Self::LegacyCommand { .. } => "legacy-command",
            Self::LegacyOp { .. } => "legacy-op",
            Self::LegacyFlag { .. } => "legacy-flag",
            Self::LegacyCommonFlag { .. } => "legacy-common-flag",
            Self::StoreScheme { .. } => "store-scheme",
            Self::StoreSetting { .. } => "store-setting",
            Self::Fetcher { .. } => "fetcher",
            Self::XpFeature { .. } => "xp-feature",
            Self::Builtin { .. } => "builtin",
            Self::WorkerOp { .. } => "worker-op",
            Self::ServeCommand { .. } => "serve-command",
            Self::MachinesField { .. } => "machines-field",
            Self::EnvVar { .. } => "env-var",
        }
    }

    pub fn subject(&self) -> String {
        match self {
            Self::NixCommand { path } => format!("nix {}", path.join(" ")),
            Self::NixFlag { path, flag, .. } => format!("nix {} --{flag}", path.join(" ")),
            Self::NixGlobalFlag { flag, .. } => format!("nix --{flag}"),
            Self::Setting { name }
            | Self::Fetcher { name }
            | Self::XpFeature { name }
            | Self::Builtin { name }
            | Self::WorkerOp { name, .. }
            | Self::ServeCommand { name, .. }
            | Self::MachinesField { name }
            | Self::EnvVar { name } => name.clone(),
            Self::LegacyCommand { bin } => bin.clone(),
            Self::LegacyOp { bin, op } => format!("{bin} {op}"),
            Self::LegacyFlag { bin, op: Some(op), flag } => format!("{bin} {op} {flag}"),
            Self::LegacyFlag { bin, op: None, flag } => format!("{bin} {flag}"),
            Self::LegacyCommonFlag { bin, flag, .. } => format!("{bin} {flag}"),
            Self::StoreScheme { scheme, .. } => format!("{scheme}://"),
            Self::StoreSetting { scheme, name, .. } => format!("{scheme}://?{name}"),
        }
    }

    pub fn id(&self) -> String {
        format!("{}:{}", self.category(), self.subject())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub oracle: String,
    pub platform: String,
    pub rows: Vec<Row>,
}

pub fn snapshot() -> Snapshot {
    serde_json::from_str(SNAPSHOT).expect("parity snapshot parses")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiver {
    pub row: String,
    pub reason: String,
}

#[derive(Debug)]
pub struct Pending {
    pub ceiling: usize,
    pub waivers: Vec<Waiver>,
}

pub fn parse_pending(text: &str) -> Result<Pending, String> {
    let mut ceiling = None;
    let mut waivers = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(v) = line.strip_prefix("pending-ceiling: ") {
            ceiling = Some(v.parse::<usize>().map_err(|e| format!("line {}: ceiling: {e}", n + 1))?);
            continue;
        }
        let body = line
            .strip_prefix("pending-parity: ")
            .ok_or_else(|| format!("line {}: not `pending-parity: <row> — <reason>`: {line}", n + 1))?;
        let (row, reason) = body
            .split_once(" — ")
            .ok_or_else(|| format!("line {}: missing ` — <reason>`: {line}", n + 1))?;
        if reason.trim().is_empty() {
            return Err(format!("line {}: empty reason for {row}", n + 1));
        }
        waivers.push(Waiver { row: row.to_string(), reason: reason.to_string() });
    }
    Ok(Pending {
        ceiling: ceiling.ok_or("no `pending-ceiling: <n>` line")?,
        waivers,
    })
}

pub const CLAIMS: &[(&str, &str)] = &[
    ("store-scheme:unix://", "sui-store StoreAccess::detect_with routes `unix://<path>` to that worker socket"),
    ("store-scheme:local://", "sui-store StoreAccess::detect_with treats `local` as the writable /nix/store"),
    ("store-scheme:http://", "sui-store BinaryCacheStore serves http substituters (http.rs)"),
    ("store-scheme:https://", "sui-store BinaryCacheStore serves https substituters (http.rs)"),
    ("fetcher:github", "sui-eval Fetcher::fetch `github` arm"),
    ("fetcher:gitlab", "sui-eval Fetcher::fetch `gitlab` arm"),
    ("fetcher:sourcehut", "sui-eval Fetcher::fetch `sourcehut` arm"),
    ("fetcher:path", "sui-eval Fetcher::fetch `path` arm"),
    ("fetcher:git", "sui-eval Fetcher::fetch `git` arm"),
    ("fetcher:tarball", "sui-eval Fetcher::fetch `tarball` arm"),
    ("fetcher:file", "sui-eval Fetcher::fetch `file` arm"),
    ("fetcher:indirect", "sui-eval flake_registry resolves `indirect` refs through the registries"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Implemented(String),
    Missing(String),
}

pub struct Sui {
    root: clap::Command,
    catalog: HashMap<String, sui_spec::cli_coverage::SuiCommandMaturity>,
    corpus: String,
    claims: HashMap<&'static str, &'static str>,
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn walk(dir: &Path, keep: &dyn Fn(&Path) -> bool, skip_dir: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            if !skip_dir(&p) {
                walk(&p, keep, skip_dir, out);
            }
        } else if keep(&p) {
            out.push(p);
        }
    }
}

fn sui_source_corpus() -> String {
    let root = manifest_dir();
    let mut files = Vec::new();
    let this = root.join("src/parity_matrix.rs");
    walk(
        &root,
        &|p| p.extension().is_some_and(|e| e == "rs") && p != this,
        &|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            matches!(name, "target" | "tests" | "benches" | "examples" | "fixtures" | "parity" | ".git" | ".claude")
                || name.starts_with("target")
        },
        &mut files,
    );
    let mut corpus = String::new();
    for f in files {
        if let Ok(text) = std::fs::read_to_string(&f) {
            corpus.push_str(&text);
            corpus.push('\n');
        }
    }
    corpus
}

impl Sui {
    pub fn new() -> Self {
        let mut root = Cli::command();
        root.build();
        let catalog = sui_spec::cli_coverage::load_canonical()
            .expect("cli coverage catalog parses")
            .into_iter()
            .map(|c| (c.name, c.maturity))
            .collect();
        Self {
            root,
            catalog,
            corpus: sui_source_corpus(),
            claims: CLAIMS.iter().copied().collect(),
        }
    }

    fn node(&self, path: &[String]) -> Option<&clap::Command> {
        let mut cur = &self.root;
        for p in path {
            cur = cur.find_subcommand(p)?;
        }
        Some(cur)
    }

    fn literal(&self, name: &str) -> Verdict {
        if self.corpus.contains(&format!("\"{name}\"")) {
            Verdict::Implemented(format!("sui source names \"{name}\""))
        } else {
            Verdict::Missing(format!("no sui source names \"{name}\""))
        }
    }

    fn refused(path: &[String], id: &str) -> Option<&'static str> {
        cli_contract::CONTRACT.iter().find_map(|(p, a, h)| {
            let same_path = p.len() == path.len() && p.iter().zip(path).all(|(x, y)| *x == y.as_str());
            let global = p.is_empty();
            match h {
                cli_contract::Honour::Refused { reason, .. } if *a == id && (same_path || global) => Some(*reason),
                _ => None,
            }
        })
    }

    fn flag_on(&self, path: &[String], flag: &str) -> Verdict {
        let Some(node) = self.node(path) else {
            return Verdict::Missing(format!("no `sui {}` subcommand", path.join(" ")));
        };
        let arg = node.get_arguments().find(|a| {
            a.get_long() == Some(flag)
                || a.get_all_aliases().is_some_and(|v| v.contains(&flag))
                || (flag.len() == 1 && a.get_short().is_some_and(|c| c.to_string() == flag))
        });
        match arg {
            None => Verdict::Missing(format!("`sui {}` declares no --{flag}", path.join(" "))),
            Some(a) => match Self::refused(path, a.get_id().as_str()) {
                Some(reason) => Verdict::Missing(format!("cli_contract refuses --{flag}: {reason}")),
                None => Verdict::Implemented(format!("`sui {}` --{flag}", path.join(" "))),
            },
        }
    }

    fn legacy_probe(&self, bin: &str, words: &[&str]) -> Verdict {
        let Some(cmd) = legacy::LegacyCmd::from_basename(bin) else {
            return Verdict::Missing(format!("argv[0] `{bin}` is not a sui LegacyCmd"));
        };
        if words == ["--version"] {
            return Verdict::Implemented("main answers --version for every nix-* argv[0]".into());
        }
        let mut last = String::new();
        for extra in 0..=2 {
            let argv: Vec<String> = words
                .iter()
                .map(|w| (*w).to_string())
                .chain(std::iter::repeat_n("x".to_string(), extra))
                .collect();
            let translated = legacy::translate_legacy_argv(cmd, &argv);
            let synthetic: Vec<String> = std::iter::once("sui".to_string()).chain(translated.iter().cloned()).collect();
            match self.root.clone().try_get_matches_from(&synthetic) {
                Ok(m) => {
                    return match cli_contract::enforce(&m) {
                        Ok(()) => Verdict::Implemented(format!("parses as `sui {}`", translated.join(" "))),
                        Err(e) => Verdict::Missing(format!("cli_contract refuses `sui {}`: {e}", translated.join(" "))),
                    };
                }
                Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => {
                    return Verdict::Implemented(format!("`sui {}` prints help", translated.join(" ")));
                }
                Err(e) => last = format!("`sui {}`: {}", translated.join(" "), e.kind()),
            }
        }
        Verdict::Missing(last)
    }

    fn builtin(name: &str) -> Verdict {
        match sui_eval::eval::eval(&format!("builtins ? \"{name}\"")) {
            Ok(sui_eval::value::Value::Bool(true)) => Verdict::Implemented("sui-eval builtins attrset".into()),
            Ok(_) => Verdict::Missing("absent from sui-eval's builtins attrset".into()),
            Err(e) => Verdict::Missing(format!("sui-eval: {e}")),
        }
    }

    pub fn verdict(&self, row: &Row) -> Verdict {
        if let Some(evidence) = self.claims.get(row.id().as_str()) {
            return Verdict::Implemented(format!("claim: {evidence}"));
        }
        match row {
            Row::NixCommand { path } => match self.node(path) {
                None => Verdict::Missing(format!("no `sui {}` subcommand", path.join(" "))),
                Some(_) => match self.catalog.get(&path.join(" ")) {
                    Some(m) if matches!(m, sui_spec::cli_coverage::SuiCommandMaturity::Stub | sui_spec::cli_coverage::SuiCommandMaturity::Missing) => {
                        Verdict::Missing(format!("cli_coverage grades it {}", m.name()))
                    }
                    _ => Verdict::Implemented(format!("`sui {}`", path.join(" "))),
                },
            },
            Row::NixFlag { path, flag, .. } => match self.verdict(&Row::NixCommand { path: path.clone() }) {
                Verdict::Missing(why) => Verdict::Missing(why),
                Verdict::Implemented(_) => self.flag_on(path, flag),
            },
            Row::NixGlobalFlag { flag, .. } => self.flag_on(&[], flag),
            Row::Setting { name } | Row::XpFeature { name } | Row::EnvVar { name } => self.literal(name),
            Row::LegacyCommand { bin } => match legacy::LegacyCmd::from_basename(bin) {
                Some(_) => Verdict::Implemented("sui LegacyCmd argv[0]".into()),
                None => Verdict::Missing(format!("argv[0] `{bin}` is not a sui LegacyCmd")),
            },
            Row::LegacyOp { bin, op } => self.legacy_probe(bin, &[op]),
            Row::LegacyFlag { bin, op: Some(op), flag } => self.legacy_probe(bin, &[op, flag]),
            Row::LegacyFlag { bin, op: None, flag } | Row::LegacyCommonFlag { bin, flag, .. } => self.legacy_probe(bin, &[flag]),
            Row::Builtin { name } => Self::builtin(name),
            Row::WorkerOp { code, .. } => match sui_compat::wire::WorkerOp::from_u64(*code) {
                Some(op) => Verdict::Implemented(format!("sui_compat::wire::WorkerOp::{op:?}")),
                None => Verdict::Missing(format!("sui_compat::wire::WorkerOp has no opcode {code}")),
            },
            Row::StoreScheme { .. }
            | Row::StoreSetting { .. }
            | Row::Fetcher { .. }
            | Row::ServeCommand { .. }
            | Row::MachinesField { .. } => Verdict::Missing("no sui claim for this row".into()),
        }
    }
}

pub mod extract {
    use super::*;
    use regex::Regex;
    use serde_json::Value;

    fn run_json(nix: &Path, args: &[&str]) -> Value {
        let out = std::process::Command::new(nix).args(args).output().expect("run cppnix");
        assert!(out.status.success(), "cppnix {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).expect("cppnix json")
    }

    fn keys(v: &Value) -> Vec<String> {
        v.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default()
    }

    fn short(f: &Value) -> Option<String> {
        f.get("shortName").and_then(Value::as_str).map(str::to_string)
    }

    fn arity(f: &Value) -> u64 {
        f.get("arity").and_then(Value::as_u64).unwrap_or(0)
    }

    fn commands(v: &Value, path: &mut Vec<String>, rows: &mut BTreeSet<Row>) {
        let Some(cmds) = v.get("commands").and_then(Value::as_object) else { return };
        for (name, cmd) in cmds {
            path.push(name.clone());
            rows.insert(Row::NixCommand { path: path.clone() });
            if let Some(flags) = cmd.get("flags").and_then(Value::as_object) {
                for (flag, f) in flags {
                    rows.insert(Row::NixFlag { path: path.clone(), flag: flag.clone(), short: short(f), arity: arity(f) });
                }
            }
            commands(cmd, path, rows);
            path.pop();
        }
    }

    fn sources(src: &Path, exts: &[&str]) -> Vec<PathBuf> {
        let mut out = Vec::new();
        walk(
            src,
            &|p| p.extension().and_then(|e| e.to_str()).is_some_and(|e| exts.contains(&e)),
            &|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains("test")),
            &mut out,
        );
        out
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
    }

    fn enum_body<'t>(text: &'t str, header: &str) -> &'t str {
        let start = text.find(header).unwrap_or_else(|| panic!("no `{header}`"));
        let rest = &text[start..];
        &rest[..rest.find("};").expect("enum end")]
    }

    fn numbered(body: &str) -> Vec<(String, u64)> {
        let re = Regex::new(r"(?m)^\s+(\w+)\s*=\s*(\d+),").unwrap();
        re.captures_iter(body).map(|c| (c[1].to_string(), c[2].parse().unwrap())).collect()
    }

    fn legacy(src: &Path, rows: &mut BTreeSet<Row>) {
        let reg = Regex::new(r#"RegisterLegacyCommand\s+\w+\("([^"]+)",\s*\w+\)"#).unwrap();
        let fn_re = Regex::new(r"^(?:static\s+)?[\w:<>&*]+\s+[*&]?(\w+)\s*\(").unwrap();
        let flag_re = Regex::new(r#"(?:\*arg|\*i|\barg|\bi)\s*==\s*"(-[^"]+)""#).unwrap();
        let op_re = Regex::new(r"\bop\s*=\s*(op\w+)").unwrap();
        let mut by_file: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
        for f in sources(&src.join("nix"), &["cc"]) {
            for c in reg.captures_iter(&read(&f)) {
                by_file.entry(f.clone()).or_default().push(c[1].to_string());
            }
        }
        for (file, bins) in &by_file {
            let text = read(file);
            let lines: Vec<&str> = text.lines().collect();
            let mut current = String::new();
            let mut occurrences: Vec<(String, String)> = Vec::new();
            let mut ops: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (n, line) in lines.iter().enumerate() {
                if let Some(c) = fn_re.captures(line) {
                    current = c[1].to_string();
                }
                let here: Vec<String> = flag_re.captures_iter(line).map(|c| c[1].to_string()).collect();
                for f in &here {
                    occurrences.push((current.clone(), f.clone()));
                }
                if let Some(c) = op_re.captures(line) {
                    let flags = if here.is_empty() && n > 0 {
                        flag_re.captures_iter(lines[n - 1]).map(|c| c[1].to_string()).collect()
                    } else {
                        here.clone()
                    };
                    if !flags.is_empty() {
                        ops.entry(c[1].to_string()).or_default().extend(flags);
                    }
                }
            }
            let op_flags: BTreeSet<&String> = ops.values().flatten().collect();
            for bin in bins {
                rows.insert(Row::LegacyCommand { bin: bin.clone() });
                for (func, flag) in &occurrences {
                    if op_flags.contains(flag) && !ops.contains_key(func) {
                        rows.insert(Row::LegacyOp { bin: bin.clone(), op: flag.clone() });
                    } else if let Some(op) = ops.get(func).and_then(|v| v.first()) {
                        rows.insert(Row::LegacyFlag { bin: bin.clone(), op: Some(op.clone()), flag: flag.clone() });
                    } else {
                        rows.insert(Row::LegacyFlag { bin: bin.clone(), op: None, flag: flag.clone() });
                    }
                }
            }
        }
        let shared = read(&src.join("libmain/shared.cc"));
        let start = shared.find("LegacyArgs::LegacyArgs(").expect("LegacyArgs ctor");
        let end = shared.find("bool LegacyArgs::processFlag").expect("LegacyArgs end");
        let ctor = &shared[start..end];
        let mut common: Vec<(String, u64)> = Vec::new();
        for block in ctor.split("addFlag({").skip(1) {
            let block = &block[..block.find("});").unwrap_or(block.len())];
            let arity = u64::from(block.contains(".labels"));
            if let Some(c) = Regex::new(r#"\.longName = "([^"]+)""#).unwrap().captures(block) {
                common.push((format!("--{}", &c[1]), arity));
            }
            if let Some(c) = Regex::new(r".shortName = '(.)'").unwrap().captures(block) {
                common.push((format!("-{}", &c[1]), arity));
            }
        }
        for c in Regex::new(r#"intSettingAlias\(\s*\w+,\s*"([^"]+)""#).unwrap().captures_iter(ctor) {
            common.push((format!("--{}", &c[1]), 1));
        }
        let bins: BTreeSet<&String> = by_file.values().flatten().collect();
        for bin in bins {
            for (flag, arity) in &common {
                rows.insert(Row::LegacyCommonFlag { bin: bin.clone(), flag: flag.clone(), arity: *arity });
            }
        }
    }

    pub fn snapshot(bin_dir: &Path, src_root: &Path) -> Snapshot {
        let nix = bin_dir.join("nix");
        let src = src_root.join("src");
        let mut rows = BTreeSet::new();

        let cli = run_json(&nix, &["__dump-cli"]);
        let config = run_json(&nix, &["config", "show", "--json"]);
        let settings: BTreeSet<String> = keys(&config).into_iter().collect();
        for s in &settings {
            rows.insert(Row::Setting { name: s.clone() });
        }
        commands(&cli["args"], &mut Vec::new(), &mut rows);
        if let Some(flags) = cli["args"]["flags"].as_object() {
            for (flag, f) in flags {
                let base = flag.strip_prefix("extra-").unwrap_or(flag);
                let base = base.strip_prefix("no-").unwrap_or(base);
                if settings.contains(flag) || settings.contains(base) {
                    continue;
                }
                rows.insert(Row::NixGlobalFlag { flag: flag.clone(), short: short(f), arity: arity(f) });
            }
        }
        if let Some(stores) = cli["stores"].as_object() {
            for (store, s) in stores {
                let schemes: Vec<String> = s["uri-schemes"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                for scheme in &schemes {
                    rows.insert(Row::StoreScheme { store: store.clone(), scheme: scheme.clone() });
                }
                let primary = schemes.first().cloned().unwrap_or_else(|| store.clone());
                for name in keys(&s["settings"]) {
                    rows.insert(Row::StoreSetting { store: store.clone(), scheme: primary.clone(), name });
                }
            }
        }
        for name in keys(&cli["fetchers"]) {
            rows.insert(Row::Fetcher { name });
        }
        for name in keys(&run_json(&nix, &["__dump-xp-features"])) {
            rows.insert(Row::XpFeature { name });
        }
        for name in keys(&run_json(&nix, &["__dump-language"])) {
            rows.insert(Row::Builtin { name });
        }

        legacy(&src, &mut rows);

        let worker = read(&src.join("libstore/include/nix/store/worker-protocol.hh"));
        for (name, code) in numbered(enum_body(&worker, "enum struct WorkerProto::Op")) {
            rows.insert(Row::WorkerOp { name, code });
        }
        let serve = read(&src.join("libstore/include/nix/store/serve-protocol.hh"));
        for (name, code) in numbered(enum_body(&serve, "enum struct ServeProto::Command")) {
            rows.insert(Row::ServeCommand { name, code });
        }
        let machines = read(&src.join("libstore/include/nix/store/machines.hh"));
        let body = enum_body(&machines, "struct Machine\n{");
        for c in Regex::new(r"(?m)^\s+const [\w:<> ]+? (\w+);").unwrap().captures_iter(body) {
            rows.insert(Row::MachinesField { name: c[1].to_string() });
        }
        let env = Regex::new(r#"getEnv(?:NonEmpty)?\("([A-Za-z][A-Za-z0-9_]*)"\)"#).unwrap();
        for f in sources(&src, &["cc", "hh"]) {
            for c in env.captures_iter(&read(&f)) {
                rows.insert(Row::EnvVar { name: c[1].to_string() });
            }
        }

        let version = std::process::Command::new(&nix).arg("--version").output().expect("nix --version");
        Snapshot {
            oracle: String::from_utf8_lossy(&version.stdout).trim().to_string(),
            platform: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            rows: rows.into_iter().collect(),
        }
    }
}

pub type Verdicts = BTreeMap<String, Verdict>;

pub fn verdicts(rows: &[Row]) -> Verdicts {
    let rows = rows.to_vec();
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(move || {
            let sui = Sui::new();
            rows.iter().map(|r| (r.id(), sui.verdict(r))).collect()
        })
        .expect("spawn verdict thread")
        .join()
        .expect("verdict thread")
}

pub fn summary(rows: &[Row], verdicts: &Verdicts) -> BTreeMap<&'static str, (usize, usize)> {
    let mut by: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    for r in rows {
        let e = by.entry(r.category()).or_default();
        match verdicts.get(&r.id()) {
            Some(Verdict::Implemented(_)) => e.0 += 1,
            _ => e.1 += 1,
        }
    }
    by
}

pub fn check(rows: &[Row], pending: &Pending, verdicts: &Verdicts) -> Vec<String> {
    let mut errors = Vec::new();
    let ids: BTreeSet<String> = rows.iter().map(Row::id).collect();
    let mut waived: BTreeMap<&str, &str> = BTreeMap::new();
    for w in &pending.waivers {
        if waived.insert(&w.row, &w.reason).is_some() {
            errors.push(format!("waived twice: {}", w.row));
        }
        if !ids.contains(&w.row) {
            errors.push(format!("waiver names no matrix row: {}", w.row));
        }
    }
    for (claim, _) in CLAIMS {
        if !ids.contains(*claim) {
            errors.push(format!("claim names no matrix row: {claim}"));
        }
    }
    for r in rows {
        let id = r.id();
        match (verdicts.get(&id), waived.contains_key(id.as_str())) {
            (Some(Verdict::Missing(why)), false) => errors.push(format!(
                "unwaived parity gap: {id} ({why}); implement it or add `pending-parity: {id} — <reason>` to {PENDING_FILE}"
            )),
            (Some(Verdict::Implemented(how)), true) => errors.push(format!(
                "{id} is implemented ({how}); remove its waiver from {PENDING_FILE} and lower pending-ceiling"
            )),
            (None, _) => errors.push(format!("no verdict computed for {id}")),
            _ => {}
        }
    }
    if pending.waivers.len() > pending.ceiling {
        errors.push(format!("pending count {} rose above pending-ceiling {}", pending.waivers.len(), pending.ceiling));
    }
    if pending.waivers.len() < pending.ceiling {
        errors.push(format!(
            "pending count {} fell below pending-ceiling {}; lower the ceiling to {} in the same commit",
            pending.waivers.len(),
            pending.ceiling,
            pending.waivers.len()
        ));
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cppnix_inputs() -> (PathBuf, PathBuf) {
        let bin = std::env::var_os("SUI_PARITY_CPPNIX_BIN_DIR")
            .map(PathBuf::from)
            .expect("SUI_PARITY_CPPNIX_BIN_DIR: a cppnix 2.31.5 bin dir");
        let src = std::env::var_os("SUI_PARITY_CPPNIX_SRC")
            .map(PathBuf::from)
            .expect("SUI_PARITY_CPPNIX_SRC: a NixOS/nix checkout at tag 2.31.5");
        (bin, src)
    }

    #[test]
    #[ignore = "regenerates parity/cppnix-2.31.5.json from a real cppnix; needs SUI_PARITY_CPPNIX_BIN_DIR and SUI_PARITY_CPPNIX_SRC"]
    fn regenerate_snapshot() {
        let (bin, src) = cppnix_inputs();
        let snap = extract::snapshot(&bin, &src);
        let json = serde_json::to_string_pretty(&snap).unwrap() + "\n";
        std::fs::write(manifest_dir().join(SNAPSHOT_FILE), json).unwrap();
    }

    #[test]
    #[ignore = "rewrites parity/pending.txt with every missing row; run once to seed, then edit by hand"]
    fn seed_pending() {
        let snap = snapshot();
        let verdicts = verdicts(&snap.rows);
        let mut lines = vec![
            "# pending-parity: <row id> — <reason>. One line per cppnix row sui does not implement.".to_string(),
            "# The gate (src/parity_matrix.rs) fails on an unwaived missing row, on a waived row sui implements,".to_string(),
            "# on a waiver naming no row, and when the count differs from pending-ceiling. The ceiling only falls.".to_string(),
        ];
        let mut waivers = Vec::new();
        for r in &snap.rows {
            if let Some(Verdict::Missing(why)) = verdicts.get(&r.id()) {
                let why = why.split_whitespace().collect::<Vec<_>>().join(" ");
                waivers.push(format!("pending-parity: {} — {why}", r.id()));
            }
        }
        lines.push(format!("pending-ceiling: {}", waivers.len()));
        lines.extend(waivers);
        std::fs::write(manifest_dir().join(PENDING_FILE), lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn snapshot_rows_are_unique_and_cover_every_category() {
        let snap = snapshot();
        let mut seen = BTreeSet::new();
        for r in &snap.rows {
            assert!(seen.insert(r.id()), "duplicate row id {}", r.id());
        }
        let cats: BTreeSet<&str> = snap.rows.iter().map(Row::category).collect();
        for c in [
            "nix-command", "nix-flag", "nix-global-flag", "setting", "legacy-command", "legacy-op", "legacy-flag",
            "legacy-common-flag", "store-scheme", "store-setting", "fetcher", "xp-feature", "builtin", "worker-op",
            "serve-command", "machines-field", "env-var",
        ] {
            assert!(cats.contains(c), "snapshot has no `{c}` rows");
        }
    }

    #[test]
    fn the_remote_store_and_builder_surfaces_are_rows() {
        let ids: BTreeSet<String> = snapshot().rows.iter().map(Row::id).collect();
        for id in [
            "legacy-flag:nix-daemon --stdio",
            "nix-flag:nix daemon --stdio",
            "legacy-op:nix-store --serve",
            "legacy-flag:nix-store --serve --write",
            "legacy-command:build-remote",
            "store-scheme:ssh-ng://",
            "store-scheme:ssh://",
            "serve-command:BuildDerivation",
            "machines-field:maxJobs",
            "setting:builders",
        ] {
            assert!(ids.contains(id), "matrix lost {id}");
        }
    }

    #[test]
    fn every_cppnix_row_is_implemented_or_waived() {
        let snap = snapshot();
        let pending = parse_pending(PENDING).expect("pending.txt parses");
        let verdicts = verdicts(&snap.rows);
        let errors = check(&snap.rows, &pending, &verdicts);
        let by = summary(&snap.rows, &verdicts);
        let total: (usize, usize) = by.values().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        eprintln!(
            "parity matrix against {} ({}): {} rows, {} implemented, {} pending",
            snap.oracle,
            snap.platform,
            snap.rows.len(),
            total.0,
            total.1
        );
        for (cat, (done, missing)) in &by {
            eprintln!("  {cat:<20} {:>5} rows  {done:>5} implemented  {missing:>5} pending", done + missing);
        }
        assert!(errors.is_empty(), "{} parity matrix errors:\n{}", errors.len(), errors.join("\n"));
    }

    #[test]
    fn the_gate_fires_on_a_dropped_waiver_a_raised_count_and_a_stale_waiver() {
        let snap = snapshot();
        let verdicts = verdicts(&snap.rows);
        let base = parse_pending(PENDING).unwrap();

        let mut dropped = parse_pending(PENDING).unwrap();
        let gone = dropped.waivers.remove(0);
        dropped.ceiling -= 1;
        let errors = check(&snap.rows, &dropped, &verdicts);
        assert!(errors.iter().any(|e| e.starts_with(&format!("unwaived parity gap: {} (", gone.row))), "{errors:?}");

        let implemented = snap
            .rows
            .iter()
            .map(Row::id)
            .find(|id| matches!(verdicts.get(id), Some(Verdict::Implemented(_))))
            .expect("some row is implemented");
        let mut raised = parse_pending(PENDING).unwrap();
        raised.waivers.push(Waiver { row: implemented.clone(), reason: "x".into() });
        let errors = check(&snap.rows, &raised, &verdicts);
        assert!(errors.iter().any(|e| e.contains("rose above pending-ceiling")), "{errors:?}");
        assert!(errors.iter().any(|e| e.starts_with(&format!("{implemented} is implemented"))), "{errors:?}");

        let mut stale = parse_pending(PENDING).unwrap();
        stale.waivers[0].row = "setting:no-such-setting".into();
        let errors = check(&snap.rows, &stale, &verdicts);
        assert!(errors.iter().any(|e| e == "waiver names no matrix row: setting:no-such-setting"), "{errors:?}");

        let mut lowered = parse_pending(PENDING).unwrap();
        lowered.ceiling = base.ceiling + 1;
        let errors = check(&snap.rows, &lowered, &verdicts);
        assert!(errors.iter().any(|e| e.contains("fell below pending-ceiling")), "{errors:?}");
    }

    #[test]
    fn waiver_lines_are_typed() {
        assert!(parse_pending("pending-ceiling: 1\npending-parity: setting:x\n").is_err());
        assert!(parse_pending("pending-ceiling: 1\npending-parity: setting:x — \n").is_err());
        assert!(parse_pending("pending-parity: setting:x — y\n").is_err());
        let p = parse_pending("pending-ceiling: 1\npending-parity: setting:x — y\n").unwrap();
        assert_eq!(p.waivers, vec![Waiver { row: "setting:x".into(), reason: "y".into() }]);
    }
}
