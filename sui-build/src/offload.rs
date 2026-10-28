use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use sui_compat::derivation::Derivation;
use sui_compat::machines::Machine;
use sui_compat::store_path::StorePath;
use sui_store::serve_client::BuildOptions;
use sui_store::ssh_store::{RemoteConn, SshOptions};
use sui_store::traits::{PathInfo, Store};
use sui_store::worker_client::{ClientOptions, RemotePathInfo, WorkerError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffloadSwitch {
    On,
    Off,
}

impl OffloadSwitch {
    pub fn parse(value: Option<&str>) -> Result<Self, OffloadError> {
        match value.map(str::trim) {
            None | Some("" | "on" | "true" | "1") => Ok(Self::On),
            Some("off" | "false" | "0") => Ok(Self::Off),
            Some(other) => Err(OffloadError::Config(format!("SUI_OFFLOAD must be 'on' or 'off', not '{other}'"))),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OffloadError {
    #[error("remote builders: {0}")]
    Config(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCapabilities {
    pub system: String,
    pub extra_platforms: BTreeSet<String>,
    pub system_features: BTreeSet<String>,
    pub max_jobs: usize,
}

impl HostCapabilities {
    #[must_use]
    pub fn native_system() -> String {
        let arch = std::env::consts::ARCH;
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        };
        format!("{arch}-{os}")
    }

    #[must_use]
    pub fn default_system_features() -> BTreeSet<String> {
        let mut f: BTreeSet<String> = ["nixos-test", "benchmark", "big-parallel"].into_iter().map(String::from).collect();
        if cfg!(target_os = "linux") {
            f.insert("uid-range".into());
            if std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok() {
                f.insert("kvm".into());
            }
        }
        if cfg!(target_os = "macos") && sysctl_flag("kern.hv_vmm_present") != Some(true) && sysctl_flag("kern.hv_support") == Some(true) {
            f.insert("apple-virt".into());
        }
        f
    }

    #[must_use]
    pub fn default_extra_platforms(system: &str) -> BTreeSet<String> {
        let mut p = BTreeSet::new();
        if system == "x86_64-linux" {
            p.insert("i686-linux".to_string());
        }
        if system == "aarch64-darwin"
            && std::process::Command::new("arch")
                .args(["-arch", "x86_64", "/usr/bin/true"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        {
            p.insert("x86_64-darwin".to_string());
        }
        p
    }
}

fn sysctl_flag(name: &str) -> Option<bool> {
    let out = std::process::Command::new("/usr/sbin/sysctl").args(["-n", name]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim() == "1")
}

#[derive(Debug, Clone)]
pub struct OffloadPolicy {
    pub switch: OffloadSwitch,
    pub machines: Vec<Machine>,
    pub host: HostCapabilities,
    pub builders_use_substitutes: bool,
    pub build_options: BuildOptions,
    pub ssh: SshOptions,
    pub real_store_dir: std::path::PathBuf,
}

impl OffloadPolicy {
    #[must_use]
    pub fn off(host: HostCapabilities) -> Self {
        Self {
            switch: OffloadSwitch::Off,
            machines: Vec::new(),
            host,
            builders_use_substitutes: false,
            build_options: BuildOptions::default(),
            ssh: SshOptions { nix_sshopts: Vec::new(), connect_timeout: None },
            real_store_dir: std::path::PathBuf::from(sui_compat::store_path::DEFAULT_STORE_DIR),
        }
    }

    pub fn from_nix_conf() -> Result<Self, OffloadError> {
        let cfg = sui_compat::nix_conf::NixConfig::load().map_err(|e| OffloadError::Config(e.to_string()))?;
        let conf_dir = std::env::var_os("NIX_CONF_DIR").map_or_else(|| std::path::PathBuf::from("/etc/nix"), std::path::PathBuf::from);
        Self::from_config(
            &cfg,
            &conf_dir,
            std::env::var("SUI_OFFLOAD").ok().as_deref(),
            std::env::var("SUI_MAX_JOBS").ok().as_deref(),
            &|p| std::fs::read_to_string(p),
        )
    }

    pub fn from_config(
        cfg: &sui_compat::nix_conf::NixConfig,
        conf_dir: &std::path::Path,
        switch: Option<&str>,
        max_jobs_override: Option<&str>,
        read: &dyn Fn(&std::path::Path) -> std::io::Result<String>,
    ) -> Result<Self, OffloadError> {
        let switch = OffloadSwitch::parse(switch)?;
        let system = cfg.get("system").map_or_else(HostCapabilities::native_system, str::to_string);
        let set = |name: &str, default: BTreeSet<String>| -> BTreeSet<String> {
            let mut value = default;
            for a in cfg.assignments() {
                if a.name == name {
                    value = a.value.split_whitespace().map(String::from).collect();
                } else if a.name == format!("extra-{name}") {
                    value.extend(a.value.split_whitespace().map(String::from));
                }
            }
            value
        };
        let system_features = set("system-features", HostCapabilities::default_system_features());
        let mut extra_platforms = HostCapabilities::default_extra_platforms(&system);
        for a in cfg.assignments() {
            if a.name == "extra-platforms" {
                extra_platforms = a.value.split_whitespace().map(String::from).collect();
            } else if a.name == "extra-extra-platforms" {
                extra_platforms.extend(a.value.split_whitespace().map(String::from));
            }
        }
        let max_jobs = parse_max_jobs(max_jobs_override.or(cfg.get("max-jobs")));
        let builders = cfg
            .get("builders")
            .map_or_else(|| sui_compat::machines::default_builders(conf_dir), str::to_string);
        let machines = if switch == OffloadSwitch::On {
            sui_compat::machines::parse_config(&BTreeSet::from([system.clone()]), &builders, read)
                .map_err(|e| OffloadError::Config(e.to_string()))?
        } else {
            Vec::new()
        };
        let int = |name: &str| cfg.get(name).and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
        Ok(Self {
            switch,
            machines,
            host: HostCapabilities { system, extra_platforms, system_features, max_jobs },
            builders_use_substitutes: cfg.get("builders-use-substitutes") == Some("true"),
            build_options: BuildOptions {
                max_silent_time: int("max-silent-time"),
                build_timeout: int("timeout"),
                max_log_size: int("max-build-log-size"),
                keep_failed: cfg.get("keep-failed") == Some("true"),
            },
            ssh: SshOptions::from_env(),
            real_store_dir: std::path::PathBuf::from(sui_compat::store_path::DEFAULT_STORE_DIR),
        })
    }

    #[must_use]
    pub fn active(&self) -> bool {
        self.switch == OffloadSwitch::On && !self.machines.is_empty()
    }

    #[must_use]
    pub fn can_build_locally(&self, req: &DrvRequirements) -> bool {
        if !req.builtin && req.system != self.host.system && !self.host.extra_platforms.contains(&req.system) {
            return false;
        }
        if self.host.max_jobs == 0 && !req.builtin {
            return false;
        }
        req.features.iter().all(|f| self.host.system_features.contains(f))
    }

    #[must_use]
    pub fn place(&self, slots: &Slots, req: &DrvRequirements, local_free: bool) -> Placement {
        let can_local = self.can_build_locally(req);
        let build_locally = req.prefer_local && can_local && self.host.max_jobs != 0;
        if !build_locally && self.active() && req.offloadable {
            let mut right_type = false;
            let mut best: Option<(usize, u32)> = None;
            for (i, m) in self.machines.iter().enumerate() {
                if slots.disabled.get(i).copied().unwrap_or(false) || !m.accepts(&req.system, &req.features) {
                    continue;
                }
                right_type = true;
                let load = slots.busy.get(i).copied().unwrap_or(0);
                if load >= m.max_jobs {
                    continue;
                }
                let better = match best {
                    None => true,
                    Some((b, bl)) => {
                        let bm = &self.machines[b];
                        let (l, r) = (load as f32 / m.speed_factor, bl as f32 / bm.speed_factor);
                        l < r || (l == r && (m.speed_factor > bm.speed_factor || (m.speed_factor == bm.speed_factor && load < bl)))
                    }
                };
                if better {
                    best = Some((i, load));
                }
            }
            if let Some((i, _)) = best {
                return Placement::Remote(i);
            }
            if right_type && !(local_free && can_local) {
                return Placement::Postpone;
            }
        }
        if self.host.max_jobs == 0 && !req.builtin {
            return Placement::Unbuildable(
                "Unable to start any build; either increase '--max-jobs' or enable remote builds.".to_string(),
            );
        }
        Placement::Local
    }
}

#[must_use]
pub fn parse_max_jobs(value: Option<&str>) -> usize {
    match value.map(str::trim) {
        Some("auto") => std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
        Some(n) => n.parse::<usize>().unwrap_or(1),
        None => 1,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrvRequirements {
    pub system: String,
    pub features: BTreeSet<String>,
    pub prefer_local: bool,
    pub builtin: bool,
    pub offloadable: bool,
}

impl DrvRequirements {
    #[must_use]
    pub fn of(drv: &Derivation) -> Self {
        let structured = drv
            .env
            .get("__json")
            .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok());
        let (mut features, prefer_local) = match &structured {
            Some(json) => {
                let mut f = BTreeSet::new();
                flatten_strings(json.get("requiredSystemFeatures"), &mut f);
                (f, json.get("preferLocalBuild").and_then(serde_json::Value::as_bool).unwrap_or(false))
            }
            None => (
                drv.env
                    .get("requiredSystemFeatures")
                    .map(|v| v.split_whitespace().map(String::from).collect())
                    .unwrap_or_default(),
                drv.env.get("preferLocalBuild").is_some_and(|v| v == "1"),
            ),
        };
        let known_paths = drv.outputs.values().all(|o| !o.path.is_empty());
        if !known_paths {
            features.insert("ca-derivations".to_string());
        }
        Self {
            system: drv.system.clone(),
            features,
            prefer_local,
            builtin: drv.builder.starts_with("builtin:"),
            offloadable: known_paths,
        }
    }
}

fn flatten_strings(value: Option<&serde_json::Value>, out: &mut BTreeSet<String>) {
    match value {
        Some(serde_json::Value::String(s)) => {
            out.insert(s.clone());
        }
        Some(serde_json::Value::Array(items)) => {
            for i in items {
                flatten_strings(Some(i), out);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    Remote(usize),
    Local,
    Postpone,
    Unbuildable(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Slots {
    pub busy: Vec<u32>,
    pub disabled: Vec<bool>,
}

impl Slots {
    #[must_use]
    pub fn for_machines(n: usize) -> Self {
        Self { busy: vec![0; n], disabled: vec![false; n] }
    }
}

pub type Guard = Box<dyn std::any::Any + Send>;

pub trait Connector: Send + Sync {
    fn open(&self, machine: &Machine, policy: &OffloadPolicy) -> Result<(RemoteConn, Guard), WorkerError>;
}

pub struct SshConnector;

impl Connector for SshConnector {
    fn open(&self, machine: &Machine, policy: &OffloadPolicy) -> Result<(RemoteConn, Guard), WorkerError> {
        let options = ClientOptions {
            max_jobs: 1,
            max_silent_time: policy.build_options.max_silent_time,
            build_cores: 0,
        };
        let session = sui_store::ssh_store::open_remote(machine, &policy.ssh, options)?;
        Ok((session.conn, Box::new(session.child)))
    }
}

pub struct Offload {
    pub policy: OffloadPolicy,
    pub slots: Mutex<Slots>,
    pub connector: Arc<dyn Connector>,
}

impl Offload {
    #[must_use]
    pub fn new(policy: OffloadPolicy) -> Self {
        Self::with_connector(policy, Arc::new(SshConnector))
    }

    #[must_use]
    pub fn with_connector(policy: OffloadPolicy, connector: Arc<dyn Connector>) -> Self {
        let slots = Mutex::new(Slots::for_machines(policy.machines.len()));
        Self { policy, slots, connector }
    }

    pub fn place(&self, drv: &Derivation, local_free: bool) -> Placement {
        let req = DrvRequirements::of(drv);
        let mut slots = self.slots.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let placed = self.policy.place(&slots, &req, local_free);
        if let Placement::Remote(i) = placed {
            slots.busy[i] += 1;
        }
        placed
    }

    pub fn release(&self, machine: usize) {
        let mut slots = self.slots.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        slots.busy[machine] = slots.busy[machine].saturating_sub(1);
    }

    pub fn disable(&self, machine: usize) {
        let mut slots = self.slots.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        slots.disabled[machine] = true;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteFailure {
    Connect(String),
    Build(String),
}

pub async fn fs_closure(store: &dyn Store, roots: &[String]) -> Result<Vec<PathInfo>, String> {
    let mut infos: HashMap<String, PathInfo> = HashMap::new();
    let mut stack: Vec<String> = roots.to_vec();
    while let Some(p) = stack.pop() {
        if infos.contains_key(&p) {
            continue;
        }
        let sp = StorePath::from_absolute_path(&p).map_err(|e| format!("'{p}' is not a store path: {e}"))?;
        let info = store
            .query_path_info(&sp)
            .await
            .map_err(|e| format!("path info of '{p}': {e}"))?
            .ok_or_else(|| format!("path '{p}' is not valid"))?;
        for r in &info.references {
            if !infos.contains_key(r) {
                stack.push(r.clone());
            }
        }
        infos.insert(p, info);
    }
    Ok(topo_sort(infos))
}

#[must_use]
pub fn topo_sort(mut infos: HashMap<String, PathInfo>) -> Vec<PathInfo> {
    let mut names: Vec<String> = infos.keys().cloned().collect();
    names.sort();
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for name in names {
        visit(&name, &infos, &mut done, &mut BTreeSet::new(), &mut out);
    }
    out.into_iter().filter_map(|n| infos.remove(&n)).collect()
}

fn visit(name: &str, infos: &HashMap<String, PathInfo>, done: &mut BTreeSet<String>, active: &mut BTreeSet<String>, out: &mut Vec<String>) {
    if done.contains(name) || !active.insert(name.to_string()) {
        return;
    }
    if let Some(info) = infos.get(name) {
        for r in &info.references {
            if r != name && infos.contains_key(r) {
                visit(r, infos, done, active, out);
            }
        }
    }
    active.remove(name);
    done.insert(name.to_string());
    out.push(name.to_string());
}

#[must_use]
pub fn to_remote(info: &PathInfo) -> RemotePathInfo {
    RemotePathInfo {
        path: info.path.clone(),
        deriver: info.deriver.clone().unwrap_or_default(),
        nar_hash: info.nar_hash.clone(),
        references: info.references.clone(),
        registration_time: u64::try_from(info.registration_time).unwrap_or(0),
        nar_size: u64::try_from(info.nar_size).unwrap_or(0),
        ultimate: false,
        sigs: info.signatures.clone(),
        ca: info.content_address.clone().unwrap_or_default(),
    }
}

#[must_use]
pub fn from_remote(info: &RemotePathInfo) -> PathInfo {
    let nar_hash = sui_compat::hash::NixHash::parse_any(sui_compat::hash::HashAlgorithm::Sha256, &info.nar_hash).map_or_else(
        |_| if info.nar_hash.contains(':') { info.nar_hash.clone() } else { format!("sha256:{}", info.nar_hash) },
        |h| format!("sha256:{}", h.to_nix32()),
    );
    PathInfo {
        path: info.path.clone(),
        nar_hash,
        nar_size: i64::try_from(info.nar_size).unwrap_or(i64::MAX),
        references: info.references.clone(),
        deriver: (!info.deriver.is_empty()).then(|| info.deriver.clone()),
        signatures: info.sigs.clone(),
        registration_time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0)),
        content_address: (!info.ca.is_empty()).then(|| info.ca.clone()),
    }
}

pub struct RemoteJob {
    pub drv_path: String,
    pub drv: Derivation,
    pub inputs: Vec<PathInfo>,
    pub drv_closure: Vec<PathInfo>,
    pub wanted: Vec<String>,
    pub substitute: bool,
}

pub fn run_remote_job(
    conn: &mut RemoteConn,
    job: &RemoteJob,
    options: BuildOptions,
    real_store_dir: &std::path::Path,
) -> Result<Vec<(PathInfo, Vec<u8>)>, RemoteFailure> {
    let build = |e: WorkerError| RemoteFailure::Build(e.to_string());
    let trusted_or_legacy = conn.trusted().unwrap_or(true);
    let mut upload: Vec<PathInfo> = job.inputs.clone();
    let mut seen: BTreeSet<String> = upload.iter().map(|i| i.path.clone()).collect();
    for info in job.drv_closure.iter().filter(|_| !trusted_or_legacy) {
        if seen.insert(info.path.clone()) {
            upload.push(info.clone());
        }
    }
    let all: Vec<String> = upload.iter().map(|i| i.path.clone()).collect();
    let valid: BTreeSet<String> = conn.query_valid_paths(&all, job.substitute).map_err(build)?.into_iter().collect();
    let missing: Vec<PathInfo> = upload.into_iter().filter(|i| !valid.contains(&i.path)).collect();
    if !missing.is_empty() {
        conn.add_paths(missing.len(), |i| {
            let info = &missing[i];
            let mut nar = Vec::with_capacity(usize::try_from(info.nar_size).unwrap_or(0));
            let base = info.path.rsplit('/').next().unwrap_or_default();
            sui_compat::nar::NarWriter::write_path(&mut nar, &real_store_dir.join(base))
                .map_err(|e| WorkerError::Protocol(format!("dumping '{}': {e}", info.path)))?;
            Ok((to_remote(info), nar))
        })
        .map_err(build)?;
    }
    if trusted_or_legacy {
        let input_srcs: Vec<String> = if job.drv.input_derivations.is_empty() {
            job.drv.input_sources.clone()
        } else {
            job.inputs.iter().map(|i| i.path.clone()).collect()
        };
        let result = conn.build_derivation(&job.drv_path, &job.drv, &input_srcs, options).map_err(build)?;
        if !result.success() {
            return Err(RemoteFailure::Build(result.error_msg));
        }
    } else {
        conn.build_paths(&job.drv_path).map_err(build)?;
    }
    let mut fetched = Vec::new();
    for out in &job.wanted {
        let info = conn
            .query_path_info(out)
            .map_err(build)?
            .ok_or_else(|| RemoteFailure::Build(format!("the remote build did not produce '{out}'")))?;
        let nar = conn.nar_from_path(out).map_err(build)?;
        fetched.push((from_remote(&info), nar));
    }
    let map: HashMap<String, PathInfo> = fetched.iter().map(|(i, _)| (i.path.clone(), i.clone())).collect();
    let order: Vec<String> = topo_sort(map).into_iter().map(|i| i.path).collect();
    let mut by_path: BTreeMap<String, (PathInfo, Vec<u8>)> = fetched.into_iter().map(|(i, n)| (i.path.clone(), (i, n))).collect();
    Ok(order.into_iter().filter_map(|p| by_path.remove(&p)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_compat::derivation::DerivationOutput;

    fn host(system: &str, jobs: usize) -> HostCapabilities {
        HostCapabilities {
            system: system.into(),
            extra_platforms: BTreeSet::new(),
            system_features: ["big-parallel".to_string()].into(),
            max_jobs: jobs,
        }
    }

    fn policy(lines: &str, system: &str, jobs: usize) -> OffloadPolicy {
        let none = |_: &std::path::Path| -> std::io::Result<String> { Err(std::io::ErrorKind::NotFound.into()) };
        let machines = sui_compat::machines::parse_config(&BTreeSet::from([system.to_string()]), lines, &none).unwrap();
        OffloadPolicy { switch: OffloadSwitch::On, machines, ..OffloadPolicy::off(host(system, jobs)) }
    }

    fn req(system: &str, features: &[&str]) -> DrvRequirements {
        DrvRequirements {
            system: system.into(),
            features: features.iter().map(|s| (*s).to_string()).collect(),
            prefer_local: false,
            builtin: false,
            offloadable: true,
        }
    }

    #[test]
    fn a_free_matching_machine_takes_the_build_even_when_local_could() {
        let p = policy("ssh://rio x86_64-linux - 2", "x86_64-linux", 4);
        assert_eq!(p.place(&Slots::for_machines(1), &req("x86_64-linux", &[]), true), Placement::Remote(0));
    }

    #[test]
    fn prefer_local_build_stays_local_when_local_can() {
        let p = policy("ssh://rio x86_64-linux", "x86_64-linux", 4);
        let r = DrvRequirements { prefer_local: true, ..req("x86_64-linux", &[]) };
        assert_eq!(p.place(&Slots::for_machines(1), &r, true), Placement::Local);
        let foreign = DrvRequirements { prefer_local: true, ..req("x86_64-linux", &[]) };
        assert_eq!(policy("ssh://rio x86_64-linux", "aarch64-darwin", 4).place(&Slots::for_machines(1), &foreign, true), Placement::Remote(0));
    }

    #[test]
    fn a_busy_right_machine_postpones_a_foreign_build_and_declines_a_native_one() {
        let p = policy("ssh://rio x86_64-linux - 1", "aarch64-darwin", 4);
        let full = Slots { busy: vec![1], disabled: vec![false] };
        assert_eq!(p.place(&full, &req("x86_64-linux", &[]), true), Placement::Postpone);
        let native = policy("ssh://cid aarch64-darwin - 1", "aarch64-darwin", 4);
        assert_eq!(native.place(&full, &req("aarch64-darwin", &[]), true), Placement::Local);
        assert_eq!(native.place(&full, &req("aarch64-darwin", &[]), false), Placement::Postpone);
    }

    #[test]
    fn features_and_mandatory_features_route_like_cppnix() {
        let p = policy("ssh://a x86_64-linux - 1 1 kvm\nssh://b x86_64-linux - 1 1 big-parallel big-parallel", "aarch64-darwin", 1);
        let s = Slots::for_machines(2);
        assert_eq!(p.place(&s, &req("x86_64-linux", &["kvm"]), true), Placement::Remote(0));
        assert_eq!(p.place(&s, &req("x86_64-linux", &["big-parallel"]), true), Placement::Remote(1));
        assert_eq!(p.place(&s, &req("x86_64-linux", &[]), true), Placement::Remote(0), "b demands big-parallel");
        assert_eq!(p.place(&s, &req("x86_64-linux", &["benchmark"]), true), Placement::Local);
    }

    #[test]
    fn the_least_loaded_machine_per_speed_wins() {
        let p = policy("ssh://slow x86_64-linux - 4 1\nssh://fast x86_64-linux - 4 4", "aarch64-darwin", 1);
        let s = Slots { busy: vec![1, 3], disabled: vec![false, false] };
        assert_eq!(p.place(&s, &req("x86_64-linux", &[]), true), Placement::Remote(1));
        let s = Slots { busy: vec![0, 3], disabled: vec![false, false] };
        assert_eq!(p.place(&s, &req("x86_64-linux", &[]), true), Placement::Remote(0));
    }

    #[test]
    fn a_disabled_machine_is_skipped() {
        let p = policy("ssh://down x86_64-linux - 4\nssh://up x86_64-linux - 4", "aarch64-darwin", 1);
        let s = Slots { busy: vec![0, 0], disabled: vec![true, false] };
        assert_eq!(p.place(&s, &req("x86_64-linux", &[]), true), Placement::Remote(1));
    }

    #[test]
    fn max_jobs_zero_means_remote_only() {
        let p = policy("ssh://rio x86_64-linux", "x86_64-linux", 0);
        assert_eq!(p.place(&Slots::for_machines(1), &req("x86_64-linux", &[]), true), Placement::Remote(0));
        let nothing = policy("", "x86_64-linux", 0);
        assert!(matches!(nothing.place(&Slots::default(), &req("x86_64-linux", &[]), true), Placement::Unbuildable(_)));
        let builtin = DrvRequirements { builtin: true, ..req("builtin", &[]) };
        assert_eq!(nothing.place(&Slots::default(), &builtin, true), Placement::Local);
    }

    #[test]
    fn the_off_switch_keeps_every_build_local() {
        let mut p = policy("ssh://rio x86_64-linux", "x86_64-linux", 4);
        p.switch = OffloadSwitch::Off;
        assert_eq!(p.place(&Slots::for_machines(1), &req("x86_64-linux", &[]), true), Placement::Local);
        assert_eq!(OffloadSwitch::parse(Some("off")), Ok(OffloadSwitch::Off));
        assert_eq!(OffloadSwitch::parse(None), Ok(OffloadSwitch::On));
        assert!(OffloadSwitch::parse(Some("maybe")).is_err());
    }

    #[test]
    fn requirements_read_env_and_structured_attrs() {
        let mut drv = Derivation {
            outputs: [("out".to_string(), DerivationOutput { path: "/nix/store/x-a".into(), hash_algo: String::new(), hash: String::new() })].into(),
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "x86_64-linux".into(),
            builder: "/bin/sh".into(),
            args: vec![],
            env: [("requiredSystemFeatures".to_string(), "kvm  big-parallel".to_string()), ("preferLocalBuild".to_string(), "1".to_string())].into(),
        };
        let r = DrvRequirements::of(&drv);
        assert_eq!(r.features, ["big-parallel".to_string(), "kvm".to_string()].into());
        assert!(r.prefer_local && !r.builtin && r.offloadable);
        drv.env = [("__json".to_string(), r#"{"requiredSystemFeatures":["nixos-test"],"preferLocalBuild":false}"#.to_string())].into();
        let r = DrvRequirements::of(&drv);
        assert_eq!(r.features, ["nixos-test".to_string()].into());
        assert!(!r.prefer_local);
        drv.outputs.get_mut("out").unwrap().path = String::new();
        let r = DrvRequirements::of(&drv);
        assert!(r.features.contains("ca-derivations") && !r.offloadable);
    }

    #[test]
    fn the_policy_reads_nix_conf_builders_and_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("nix.conf"), "builders = @/m ; ssh-ng://cid aarch64-darwin\nmax-jobs = 0\nextra-system-features = kvm\nbuilders-use-substitutes = true\n").unwrap();
        let cfg = sui_compat::nix_conf::NixConfig::load_from(&sui_compat::nix_conf::ConfigSources {
            system: dir.path().join("nix.conf"),
            user: vec![],
            nix_config: None,
        })
        .unwrap();
        let read = |p: &std::path::Path| -> std::io::Result<String> {
            if p == std::path::Path::new("/m") { Ok("ssh://rio x86_64-linux".into()) } else { Err(std::io::ErrorKind::NotFound.into()) }
        };
        let p = OffloadPolicy::from_config(&cfg, dir.path(), None, None, &read).unwrap();
        let hosts: Vec<&str> = p.machines.iter().map(|m| m.store_uri.host.as_str()).collect();
        assert_eq!(hosts, ["rio", "cid"]);
        assert_eq!(p.host.max_jobs, 0);
        assert!(p.host.system_features.contains("kvm") && p.host.system_features.contains("big-parallel"));
        assert!(p.builders_use_substitutes);
        let off = OffloadPolicy::from_config(&cfg, dir.path(), Some("off"), Some("2"), &read).unwrap();
        assert!(off.machines.is_empty() && !off.active());
        assert_eq!(off.host.max_jobs, 2);
    }

    #[test]
    fn closures_sort_references_first() {
        let info = |p: &str, refs: &[&str]| PathInfo {
            path: p.into(),
            nar_hash: String::new(),
            nar_size: 0,
            references: refs.iter().map(|s| (*s).to_string()).collect(),
            deriver: None,
            signatures: vec![],
            registration_time: 0,
            content_address: None,
        };
        let map: HashMap<String, PathInfo> = [info("c", &["b", "c"]), info("b", &["a"]), info("a", &[])]
            .into_iter()
            .map(|i| (i.path.clone(), i))
            .collect();
        let order: Vec<String> = topo_sort(map).into_iter().map(|i| i.path).collect();
        assert_eq!(order, ["a", "b", "c"]);
    }
}
