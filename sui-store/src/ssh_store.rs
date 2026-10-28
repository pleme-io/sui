use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sui_compat::machines::{Machine, Scheme, StoreUri};

use crate::serve_client::{BuildOptions, ServeConn};
use crate::worker_client::{ClientOptions, RemoteBuildResult, RemotePathInfo, WorkerConn, WorkerError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshInvocation {
    pub program: String,
    pub args: Vec<String>,
    pub known_hosts: Option<(PathBuf, String)>,
    pub fake: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshOptions {
    pub nix_sshopts: Vec<String>,
    pub connect_timeout: Option<Duration>,
}

impl SshOptions {
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            nix_sshopts: std::env::var("NIX_SSHOPTS").map(|s| shell_split(&s)).unwrap_or_default(),
            connect_timeout: Some(Duration::from_secs(
                std::env::var("SUI_BUILDER_CONNECT_TIMEOUT").ok().and_then(|v| v.parse().ok()).unwrap_or(10),
            )),
        }
    }
}

#[must_use]
pub fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                have = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    have = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            (None, c) => {
                cur.push(c);
                have = true;
            }
        }
    }
    if have {
        out.push(cur);
    }
    out
}

#[must_use]
pub fn remote_command(uri: &StoreUri) -> Vec<String> {
    let (default, stdio) = match uri.scheme {
        Scheme::SshNg => ("nix-daemon", Some("--stdio")),
        _ => ("nix-store", None),
    };
    let mut command: Vec<String> = match uri.params.get("remote-program") {
        Some(p) if !p.is_empty() => p.split_whitespace().map(String::from).collect(),
        _ => vec![default.to_string()],
    };
    match stdio {
        Some(flag) => command.push(flag.to_string()),
        None => {
            command.push("--serve".to_string());
            command.push("--write".to_string());
        }
    }
    if let Some(store) = uri.params.get("remote-store").filter(|s| !s.is_empty()) {
        command.push("--store".to_string());
        command.push(store.clone());
    }
    command
}

#[must_use]
pub fn invocation(machine: &Machine, opts: &SshOptions, scratch: &std::path::Path) -> SshInvocation {
    let uri = &machine.store_uri;
    let command = remote_command(uri);
    let authority = match uri.port {
        Some(p) => format!("{}:{p}", uri.host_and_user()),
        None => uri.host_and_user(),
    };
    if authority == "localhost" {
        let mut it = command.into_iter();
        let program = it.next().unwrap_or_default();
        return SshInvocation { program, args: it.collect(), known_hosts: None, fake: true };
    }
    let key = uri.params.get("ssh-key").cloned().unwrap_or_else(|| machine.ssh_key.clone());
    let host_key = uri
        .params
        .get("base64-ssh-public-host-key")
        .cloned()
        .unwrap_or_else(|| machine.ssh_public_host_key.clone());
    let mut args = vec![uri.host_and_user(), "-x".to_string()];
    args.extend(opts.nix_sshopts.iter().cloned());
    if !key.is_empty() {
        args.push("-i".to_string());
        args.push(key);
    }
    let mut known_hosts = None;
    if !host_key.is_empty() {
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&host_key)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let file = scratch.join(format!("sui-known-hosts-{}-{}", std::process::id(), uri.host));
        args.push(format!("-oUserKnownHostsFile={}", file.display()));
        known_hosts = Some((file, format!("{} {}\n", uri.host, decoded.trim_end())));
    }
    if uri.params.get("compress").is_some_and(|v| v == "true") {
        args.push("-C".to_string());
    }
    if let Some(port) = uri.port {
        args.push(format!("-p{port}"));
    }
    if let Some(t) = opts.connect_timeout {
        args.push(format!("-oConnectTimeout={}", t.as_secs().max(1)));
    }
    args.push("-oPermitLocalCommand=yes".to_string());
    args.push("-oLocalCommand=echo started".to_string());
    args.push("--".to_string());
    args.extend(command);
    SshInvocation { program: "ssh".to_string(), args, known_hosts, fake: false }
}

pub struct SshChild {
    child: Child,
    stderr: Arc<Mutex<String>>,
    known_hosts: Option<PathBuf>,
}

impl SshChild {
    #[must_use]
    pub fn stderr_text(&self) -> String {
        self.stderr.lock().map(|s| s.trim().to_string()).unwrap_or_default()
    }
}

impl Drop for SshChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(f) = &self.known_hosts {
            let _ = std::fs::remove_file(f);
        }
    }
}

pub type Pipes = (Box<dyn Read + Send>, Box<dyn std::io::Write + Send>);

pub fn spawn(inv: &SshInvocation) -> Result<(Pipes, SshChild), WorkerError> {
    if let Some((file, line)) = &inv.known_hosts {
        std::fs::write(file, line)?;
    }
    let mut command = Command::new(&inv.program);
    command
        .args(&inv.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("DISPLAY")
        .env_remove("SSH_ASKPASS");
    let mut child = command
        .spawn()
        .map_err(|e| WorkerError::Protocol(format!("unable to execute '{}': {e}", inv.program)))?;
    let stderr = Arc::new(Mutex::new(String::new()));
    if let Some(mut err) = child.stderr.take() {
        let sink = stderr.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = err.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Ok(mut s) = sink.lock() {
                    s.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            }
        });
    }
    let stdin = child.stdin.take().ok_or_else(|| WorkerError::Protocol("ssh child has no stdin".into()))?;
    let stdout = child.stdout.take().ok_or_else(|| WorkerError::Protocol("ssh child has no stdout".into()))?;
    let guard = SshChild { child, stderr, known_hosts: inv.known_hosts.as_ref().map(|(f, _)| f.clone()) };
    let mut reader = BufReader::new(stdout);
    if !inv.fake {
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        if line.trim_end() != "started" {
            std::thread::sleep(Duration::from_millis(50));
            let host = inv.args.first().cloned().unwrap_or_default();
            let detail = guard.stderr_text();
            return Err(WorkerError::Protocol(format!(
                "failed to start SSH connection to '{host}'{}",
                if detail.is_empty() { String::new() } else { format!(": {detail}") }
            )));
        }
    }
    let reader: Box<dyn Read + Send> = Box::new(reader);
    let writer: Box<dyn std::io::Write + Send> = Box::new(stdin);
    Ok(((reader, writer), guard))
}

pub enum RemoteConn {
    Worker(WorkerConn),
    Serve(ServeConn),
}

impl RemoteConn {
    pub fn over(scheme: &Scheme, pipes: Pipes, client: ClientOptions, host: &str) -> Result<Self, WorkerError> {
        match scheme {
            Scheme::SshNg => Ok(Self::Worker(WorkerConn::over_transport(pipes, client)?)),
            Scheme::Ssh => Ok(Self::Serve(ServeConn::over_transport(pipes, host)?)),
            Scheme::Other(s) => Err(WorkerError::Protocol(format!("sui builds remotely only over ssh:// and ssh-ng://, not '{s}'"))),
        }
    }

    #[must_use]
    pub fn trusted(&self) -> Option<bool> {
        match self {
            Self::Worker(c) => c.trusted(),
            Self::Serve(_) => None,
        }
    }

    pub fn query_valid_paths(&mut self, paths: &[String], substitute: bool) -> Result<Vec<String>, WorkerError> {
        match self {
            Self::Worker(c) => c.query_valid_paths(paths, substitute),
            Self::Serve(c) => c.query_valid_paths(paths, false, substitute),
        }
    }

    pub fn query_path_info(&mut self, path: &str) -> Result<Option<RemotePathInfo>, WorkerError> {
        match self {
            Self::Worker(c) => c.query_path_info(path),
            Self::Serve(c) => Ok(c.query_path_infos(&[path.to_string()])?.into_iter().find(|i| i.path == path)),
        }
    }

    pub fn add_to_store_nar(&mut self, info: &RemotePathInfo, nar: &[u8]) -> Result<(), WorkerError> {
        match self {
            Self::Worker(c) => c.add_to_store_nar(info, nar, true),
            Self::Serve(c) => c.add_to_store_nar(info, nar),
        }
    }

    pub fn add_paths<I>(&mut self, count: usize, mut items: I) -> Result<(), WorkerError>
    where
        I: FnMut(usize) -> Result<(RemotePathInfo, Vec<u8>), WorkerError> + Send,
    {
        match self {
            Self::Worker(c) => c.add_multiple_to_store(count, items, true),
            Self::Serve(c) => {
                for i in 0..count {
                    let (info, nar) = items(i)?;
                    c.add_to_store_nar(&info, &nar)?;
                }
                Ok(())
            }
        }
    }

    pub fn nar_from_path(&mut self, path: &str) -> Result<Vec<u8>, WorkerError> {
        match self {
            Self::Worker(c) => c.nar_from_path(path),
            Self::Serve(c) => c.nar_from_path(path),
        }
    }

    pub fn build_derivation(
        &mut self,
        drv_path: &str,
        drv: &sui_compat::derivation::Derivation,
        input_srcs: &[String],
        options: BuildOptions,
    ) -> Result<RemoteBuildResult, WorkerError> {
        match self {
            Self::Worker(c) => c.build_derivation(drv_path, drv, input_srcs),
            Self::Serve(c) => c.build_derivation(drv_path, drv, input_srcs, options),
        }
    }

    pub fn build_paths(&mut self, drv_path: &str) -> Result<(), WorkerError> {
        match self {
            Self::Worker(c) => c.build_paths(&[drv_path]),
            Self::Serve(_) => Err(WorkerError::Protocol("BuildPaths over ssh:// is not used by the build hook".into())),
        }
    }
}

pub struct RemoteSession {
    pub conn: RemoteConn,
    pub child: SshChild,
}

pub fn open_remote(machine: &Machine, opts: &SshOptions, client: ClientOptions) -> Result<RemoteSession, WorkerError> {
    if let Scheme::Other(s) = &machine.store_uri.scheme {
        return Err(WorkerError::Protocol(format!("sui builds remotely only over ssh:// and ssh-ng://, not '{s}'")));
    }
    let inv = invocation(machine, opts, &std::env::temp_dir());
    let (pipes, child) = spawn(&inv)?;
    match RemoteConn::over(&machine.store_uri.scheme, pipes, client, &machine.store_uri.host) {
        Ok(conn) => Ok(RemoteSession { conn, child }),
        Err(e) => {
            std::thread::sleep(Duration::from_millis(50));
            let detail = child.stderr_text();
            Err(WorkerError::Protocol(if detail.is_empty() { e.to_string() } else { format!("{e}: {detail}") }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn machine(line: &str) -> Machine {
        let none = |_: &std::path::Path| -> std::io::Result<String> { Err(std::io::ErrorKind::NotFound.into()) };
        sui_compat::machines::parse_config(&BTreeSet::new(), line, &none).unwrap().remove(0)
    }

    fn opts() -> SshOptions {
        SshOptions { nix_sshopts: vec!["-oBatchMode=yes".into()], connect_timeout: Some(Duration::from_secs(5)) }
    }

    #[test]
    fn ssh_ng_runs_the_daemon_on_stdio_with_cppnix_ssh_flags() {
        let inv = invocation(&machine("ssh-ng://root@cid:2222 aarch64-darwin /k/id"), &opts(), std::path::Path::new("/tmp"));
        assert!(!inv.fake);
        assert_eq!(inv.program, "ssh");
        assert_eq!(
            inv.args,
            [
                "root@cid", "-x", "-oBatchMode=yes", "-i", "/k/id", "-p2222", "-oConnectTimeout=5",
                "-oPermitLocalCommand=yes", "-oLocalCommand=echo started", "--", "nix-daemon", "--stdio"
            ]
        );
    }

    #[test]
    fn ssh_runs_nix_store_serve_write_and_remote_params_apply() {
        let inv = invocation(&machine("ssh://cid?remote-program=/r/nix-store&remote-store=/x"), &opts(), std::path::Path::new("/tmp"));
        let tail: Vec<&str> = inv.args.iter().skip_while(|a| *a != "--").skip(1).map(String::as_str).collect();
        assert_eq!(tail, ["/r/nix-store", "--serve", "--write", "--store", "/x"]);
    }

    #[test]
    fn localhost_runs_the_command_without_ssh() {
        let inv = invocation(&machine("ssh-ng://localhost?remote-program=/bin/sui%20x"), &opts(), std::path::Path::new("/tmp"));
        assert!(inv.fake);
        assert_eq!((inv.program.as_str(), inv.args.as_slice()), ("/bin/sui", &["x".to_string(), "--stdio".to_string()][..]));
        let inv = invocation(&machine("ssh-ng://localhost?remote-program=sui"), &opts(), std::path::Path::new("/tmp"));
        assert_eq!((inv.program.as_str(), inv.args.as_slice()), ("sui", &["--stdio".to_string()][..]));
    }

    #[test]
    fn a_host_key_column_pins_known_hosts() {
        let inv = invocation(&machine("ssh-ng://cid - - - - - - c3NoLWVkMjU1MTkgQUFBQQ=="), &opts(), std::path::Path::new("/s"));
        let (file, line) = inv.known_hosts.unwrap();
        assert!(inv.args.contains(&format!("-oUserKnownHostsFile={}", file.display())));
        assert_eq!(line, "cid ssh-ed25519 AAAA\n");
    }

    #[test]
    fn nix_sshopts_split_like_a_shell() {
        assert_eq!(shell_split(r#"-o "Proxy Command=x y" -p 22 'a b'"#), ["-o", "Proxy Command=x y", "-p", "22", "a b"]);
        assert!(shell_split("   ").is_empty());
    }

    #[test]
    fn an_unreachable_host_fails_fast_with_its_ssh_error() {
        let inv = SshInvocation {
            program: "sh".into(),
            args: vec!["-c".into(), "echo nope >&2; exit 255".into()],
            known_hosts: None,
            fake: false,
        };
        let start = std::time::Instant::now();
        let err = spawn(&inv).err().expect("no started line is a failure");
        assert!(err.to_string().contains("failed to start SSH connection"), "{err}");
        assert!(err.to_string().contains("nope"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
