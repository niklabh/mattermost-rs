//! The host side: launching a plugin process, or reattaching to a running one (client.go).

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWrite, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch};

use crate::hclog::{StderrParser, emit};
use crate::rpc::{Dispensed, RpcClient, RpcError};
use crate::yamux;

/// go-plugin's `CoreProtocolVersion`.
pub const CORE_PROTOCOL_VERSION: u32 = 1;

/// Shared by host and plugin; both sides must agree (plugin.go, `HandshakeConfig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeConfig {
    /// The application's plugin protocol version (not go-plugin's own).
    pub protocol_version: u32,
    /// An environment variable the host sets, so a plugin binary run by hand says so and exits.
    pub magic_cookie_key: String,
    pub magic_cookie_value: String,
}

/// Where a plugin listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginAddr {
    Unix(PathBuf),
    Tcp(std::net::SocketAddr),
}

impl PluginAddr {
    pub fn network(&self) -> &'static str {
        match self {
            PluginAddr::Unix(_) => "unix",
            PluginAddr::Tcp(_) => "tcp",
        }
    }
}

impl std::fmt::Display for PluginAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginAddr::Unix(p) => write!(f, "{}", p.display()),
            PluginAddr::Tcp(a) => write!(f, "{a}"),
        }
    }
}

/// Enough to reattach to a running plugin (client.go, `ReattachConfig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReattachConfig {
    pub protocol: String,
    pub protocol_version: u32,
    pub addr: PluginAddr,
    pub pid: u32,
    /// A plugin started in test mode: killing the client leaves the process alone.
    pub test: bool,
}

/// The executable to launch.
#[derive(Debug, Clone)]
pub struct PluginCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub current_dir: Option<PathBuf>,
}

impl PluginCommand {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            current_dir: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }
}

/// How to start a plugin (client.go, `ClientConfig`). Defaults follow `NewClient`.
pub struct ClientConfig {
    pub handshake: HandshakeConfig,
    /// Every application protocol version this host speaks; the plugin picks one. Defaults to
    /// `handshake.protocol_version` alone.
    pub protocol_versions: Vec<u32>,
    /// Launch this. Exactly one of `cmd` and `reattach` must be set.
    pub cmd: Option<PluginCommand>,
    pub reattach: Option<ReattachConfig>,
    /// The executable's expected SHA-256. Refused together with `reattach`.
    pub checksum: Option<Vec<u8>>,
    pub start_timeout: Duration,
    pub min_port: u16,
    pub max_port: u16,
    /// Do not pass this process's environment to the plugin.
    pub skip_host_env: bool,
    /// `PLUGIN_UNIX_SOCKET_GROUP`: the group the plugin makes its socket writable by.
    pub unix_socket_group: Option<String>,
    /// Receives what the plugin writes to its standard output and error over the connection.
    pub sync_stdout: Box<dyn AsyncWrite + Send + Unpin>,
    pub sync_stderr: Box<dyn AsyncWrite + Send + Unpin>,
    /// Receives every line of the plugin process's own stderr, verbatim, before it is logged.
    pub stderr: Box<dyn AsyncWrite + Send + Unpin>,
    /// The name plugin log events carry.
    pub name: String,
    pub yamux: yamux::Config,
}

impl ClientConfig {
    pub fn new(handshake: HandshakeConfig) -> Self {
        let versions = vec![handshake.protocol_version];
        Self {
            handshake,
            protocol_versions: versions,
            cmd: None,
            reattach: None,
            checksum: None,
            start_timeout: Duration::from_secs(60),
            min_port: 10000,
            max_port: 25000,
            skip_host_env: false,
            unix_socket_group: None,
            sync_stdout: Box::new(tokio::io::sink()),
            sync_stderr: Box::new(tokio::io::sink()),
            stderr: Box::new(tokio::io::sink()),
            name: "plugin".into(),
            yamux: yamux::Config::default(),
        }
    }
}

/// Starting or talking to a plugin failed. Messages are go-plugin's.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    #[error("exactly one of Cmd, or Reattach, or RunnerFunc must be set")]
    Misconfigured,
    #[error("only one of Reattach or SecureConfig can be set")]
    SecureConfigAndReattach,
    #[error("checksums did not match")]
    ChecksumsDoNotMatch,
    #[error("error verifying checksum: {0}")]
    Checksum(std::io::Error),
    #[error("timeout while waiting for plugin to start")]
    StartTimeout,
    #[error("plugin exited before we could connect")]
    ExitedBeforeConnect,
    #[error("Unrecognized remote plugin message: {0}")]
    Unrecognized(String),
    #[error("error parsing core protocol version: {0}")]
    CoreVersionParse(String),
    #[error(
        "incompatible core API version with plugin. Plugin version: {0}, Core version: 1\n\nTo fix this, the plugin usually only needs to be recompiled.\nPlease report this to the plugin author"
    )]
    CoreVersion(String),
    #[error("Error parsing protocol version {0:?}: {1}")]
    VersionParse(String, String),
    #[error(
        "incompatible API version with plugin. Plugin version: {plugin}, Client versions: {client}"
    )]
    IncompatibleVersion { plugin: u32, client: String },
    #[error("unknown address type: {0}")]
    UnknownAddress(String),
    #[error("unsupported plugin protocol {0:?}. Supported: [netrpc]")]
    UnsupportedProtocol(String),
    #[error("the plugin requested automatic mTLS, which this host does not support")]
    AutoMtls,
    #[error("plugin process not found")]
    ProcessNotFound,
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A running plugin: its process (unless reattached in test mode) and its connection.
pub struct Client {
    rpc: RpcClient,
    addr: PluginAddr,
    pid: u32,
    negotiated_version: u32,
    exited: watch::Receiver<bool>,
    kill_tx: Mutex<Option<oneshot::Sender<()>>>,
    /// client.go's `processKilled`: `kill` had to kill the process rather than let it quit.
    force_killed: std::sync::atomic::AtomicBool,
    reattached_test: bool,
}

impl Client {
    /// Launch or reattach, per `config` (client.go, `Start` then `Client`).
    pub async fn start(config: ClientConfig) -> Result<Self, ClientError> {
        match (&config.cmd, &config.reattach) {
            (Some(_), None) => launch(config).await,
            (None, Some(_)) if config.checksum.is_some() => {
                Err(ClientError::SecureConfigAndReattach)
            }
            (None, Some(_)) => reattach(config).await,
            _ => Err(ClientError::Misconfigured),
        }
    }

    pub fn rpc(&self) -> &RpcClient {
        &self.rpc
    }

    pub async fn dispense(&self, name: &str) -> Result<Dispensed, ClientError> {
        Ok(self.rpc.dispense(name).await?)
    }

    pub async fn ping(&self) -> Result<(), ClientError> {
        Ok(self.rpc.ping().await?)
    }

    /// The application protocol version agreed with the plugin.
    pub fn negotiated_version(&self) -> u32 {
        self.negotiated_version
    }

    pub fn protocol(&self) -> &'static str {
        "netrpc"
    }

    pub fn exited(&self) -> bool {
        *self.exited.borrow()
    }

    /// What another host needs to reattach (client.go, `ReattachConfig`).
    pub fn reattach_config(&self) -> ReattachConfig {
        ReattachConfig {
            protocol: "netrpc".into(),
            protocol_version: 0,
            addr: self.addr.clone(),
            pid: self.pid,
            test: false,
        }
    }

    /// client.go, `Kill`: ask the plugin to quit, give it two seconds, then kill it.
    pub async fn kill(&self) {
        let graceful = self.rpc.close().await.is_ok();
        let mut exited = self.exited.clone();
        if graceful
            && tokio::time::timeout(Duration::from_secs(2), exited.wait_for(|e| *e))
                .await
                .is_ok()
        {
            return;
        }
        tracing::warn!("plugin failed to exit gracefully");
        if self.reattached_test {
            return;
        }
        if let Some(tx) = self
            .kill_tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = tx.send(());
        }
        self.force_killed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = exited.wait_for(|e| *e).await;
    }

    /// Whether [`Client::kill`] had to kill the process because it did not exit within two
    /// seconds of `Control.Quit`.
    pub fn force_killed(&self) -> bool {
        self.force_killed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// supervisor.go, `getPluginExecutableChecksum`: the SHA-256 of a file.
pub fn sha256_file(path: &Path) -> std::io::Result<Vec<u8>> {
    Ok(Sha256::digest(std::fs::read(path)?).to_vec())
}

fn versions_string(versions: &[u32]) -> String {
    // fmt's `%d` of a Go []int: "[1 2]".
    let mut s = String::from("[");
    for (i, v) in versions.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{v}");
    }
    s.push(']');
    s
}

async fn launch(config: ClientConfig) -> Result<Client, ClientError> {
    let Some(cmd_spec) = config.cmd.as_ref() else {
        return Err(ClientError::Misconfigured);
    };
    let mut cmd = Command::new(&cmd_spec.program);
    cmd.args(&cmd_spec.args);
    if config.skip_host_env {
        cmd.env_clear();
    }
    cmd.envs(cmd_spec.env.iter().map(|(k, v)| (k, v)));
    if let Some(dir) = &cmd_spec.current_dir {
        cmd.current_dir(dir);
    }
    let versions: Vec<String> = config
        .protocol_versions
        .iter()
        .map(u32::to_string)
        .collect();
    cmd.env(
        &config.handshake.magic_cookie_key,
        &config.handshake.magic_cookie_value,
    )
    .env("PLUGIN_MIN_PORT", config.min_port.to_string())
    .env("PLUGIN_MAX_PORT", config.max_port.to_string())
    .env("PLUGIN_PROTOCOL_VERSIONS", versions.join(","));
    if let Some(group) = &config.unix_socket_group {
        cmd.env("PLUGIN_UNIX_SOCKET_GROUP", group);
    }

    if let Some(want) = &config.checksum {
        let got = sha256_file(&cmd_spec.program).map_err(ClientError::Checksum)?;
        if !constant_time_eq(&got, want) {
            return Err(ClientError::ChecksumsDoNotMatch);
        }
    }

    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child: Child = cmd.spawn()?;
    let pid = child.id().unwrap_or(0);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let ClientConfig {
        stderr: mut raw_stderr,
        name,
        start_timeout,
        sync_stdout,
        sync_stderr,
        yamux: yamux_config,
        protocol_versions,
        ..
    } = config;

    // The process's stderr: copied verbatim, then logged (client.go, `logStderr`).
    if let Some(stderr) = stderr {
        let name = name.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut lines = BufReader::with_capacity(64 * 1024, stderr).lines();
            let mut parser = StderrParser::default();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = raw_stderr.write_all(line.as_bytes()).await;
                let _ = raw_stderr.write_all(b"\n").await;
                let _ = raw_stderr.flush().await;
                emit(&name, &parser.parse(&line));
            }
        });
    }

    // The process's lifetime, and the only owner of its handle.
    let (exited_tx, exited_rx) = watch::channel(false);
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    {
        let name = name.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                s = child.wait() => s,
                _ = kill_rx => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            match status {
                Ok(s) if s.success() => tracing::info!(plugin = name, pid, "plugin process exited"),
                Ok(s) => tracing::error!(plugin = name, pid, status = %s, "plugin process exited"),
                Err(e) => tracing::error!(plugin = name, pid, error = %e, "plugin process exited"),
            }
            exited_tx.send_replace(true);
        });
    }

    // The protocol line.
    let (line_tx, mut line_rx) = mpsc::channel::<Option<String>>(1);
    if let Some(stdout) = stdout {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let first = lines.next_line().await.ok().flatten();
            let _ = line_tx.send(first).await;
            // Later lines are drained and dropped, as go-plugin does.
            while let Ok(Some(_)) = lines.next_line().await {}
        });
    }
    let mut exited = exited_rx.clone();
    let result: Result<Client, ClientError> = async {
        let line = tokio::select! {
            l = line_rx.recv() => l.flatten(),
            _ = tokio::time::sleep(start_timeout) => return Err(ClientError::StartTimeout),
            _ = exited.wait_for(|e| *e) => return Err(ClientError::ExitedBeforeConnect),
        };
        let Some(line) = line else {
            return Err(ClientError::Unrecognized(
                "\nFailed to read any lines from plugin's stdout".into(),
            ));
        };
        let (version, addr) = parse_protocol_line(line.trim(), &protocol_versions)?;
        let rpc = connect(&addr, yamux_config, sync_stdout, sync_stderr).await?;
        Ok(Client {
            rpc,
            addr,
            pid,
            negotiated_version: version,
            exited: exited_rx.clone(),
            kill_tx: Mutex::new(None),
            force_killed: false.into(),
            reattached_test: false,
        })
    }
    .await;
    match result {
        Ok(mut c) => {
            c.kill_tx = Mutex::new(Some(kill_tx));
            Ok(c)
        }
        Err(e) => {
            let _ = kill_tx.send(());
            Err(e)
        }
    }
}

/// client.go, `Start`: `CORE|APP|network|address|protocol|servercert`.
pub fn parse_protocol_line(line: &str, versions: &[u32]) -> Result<(u32, PluginAddr), ClientError> {
    let parts: Vec<&str> = line.split('|').collect();
    if parts.len() < 4 {
        return Err(ClientError::Unrecognized(line.to_owned()));
    }
    let core: i64 = parts[0]
        .parse()
        .map_err(|e: std::num::ParseIntError| ClientError::CoreVersionParse(e.to_string()))?;
    if core != i64::from(CORE_PROTOCOL_VERSION) {
        return Err(ClientError::CoreVersion(parts[0].to_owned()));
    }
    let plugin_version: u32 = parts[1].parse().map_err(|e: std::num::ParseIntError| {
        ClientError::VersionParse(parts[1].to_owned(), e.to_string())
    })?;
    if !versions.contains(&plugin_version) {
        return Err(ClientError::IncompatibleVersion {
            plugin: plugin_version,
            client: versions_string(versions),
        });
    }
    let addr = match parts[2] {
        "unix" => PluginAddr::Unix(PathBuf::from(parts[3])),
        "tcp" => PluginAddr::Tcp(
            parts[3]
                .parse()
                .map_err(|_| ClientError::UnknownAddress(parts[3].to_owned()))?,
        ),
        _ => return Err(ClientError::UnknownAddress(parts[3].to_owned())),
    };
    let protocol = parts.get(4).copied().unwrap_or("netrpc");
    if protocol != "netrpc" {
        return Err(ClientError::UnsupportedProtocol(protocol.to_owned()));
    }
    if parts.get(5).is_some_and(|c| c.len() > 50) {
        return Err(ClientError::AutoMtls);
    }
    Ok((plugin_version, addr))
}

async fn connect(
    addr: &PluginAddr,
    config: yamux::Config,
    stdout: Box<dyn AsyncWrite + Send + Unpin>,
    stderr: Box<dyn AsyncWrite + Send + Unpin>,
) -> Result<RpcClient, ClientError> {
    Ok(match addr {
        #[cfg(unix)]
        PluginAddr::Unix(path) => {
            RpcClient::connect(
                tokio::net::UnixStream::connect(path).await?,
                config,
                stdout,
                stderr,
            )
            .await?
        }
        #[cfg(not(unix))]
        PluginAddr::Unix(_) => return Err(ClientError::UnknownAddress(addr.to_string())),
        PluginAddr::Tcp(a) => {
            let s = tokio::net::TcpStream::connect(a).await?;
            s.set_nodelay(true)?;
            RpcClient::connect(s, config, stdout, stderr).await?
        }
    })
}

async fn reattach(config: ClientConfig) -> Result<Client, ClientError> {
    let Some(r) = config.reattach.clone() else {
        return Err(ClientError::Misconfigured);
    };
    // cmdrunner.ReattachFunc: the process must exist and the address must answer.
    if !pid_alive(r.pid) {
        return Err(ClientError::ProcessNotFound);
    }
    let ClientConfig {
        sync_stdout,
        sync_stderr,
        yamux: yamux_config,
        ..
    } = config;
    let rpc = connect(&r.addr, yamux_config, sync_stdout, sync_stderr)
        .await
        .map_err(|_| ClientError::ProcessNotFound)?;
    let (exited_tx, exited_rx) = watch::channel(false);
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    let pid = r.pid;
    let test = r.test;
    tokio::spawn(async move {
        let mut kill_rx = Some(kill_rx);
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        loop {
            let kill = async {
                match kill_rx.as_mut() {
                    Some(rx) => rx.await.is_ok(),
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = ticker.tick() => {
                    if !pid_alive(pid) {
                        break;
                    }
                }
                requested = kill => {
                    if requested && !test {
                        kill_pid(pid);
                    }
                    kill_rx = None;
                }
            }
        }
        tracing::debug!("reattached plugin process exited");
        exited_tx.send_replace(true);
    });
    Ok(Client {
        rpc,
        addr: r.addr,
        pid,
        negotiated_version: if test { r.protocol_version } else { 0 },
        exited: exited_rx,
        kill_tx: Mutex::new(Some(kill_tx)),
        force_killed: false.into(),
        reattached_test: test,
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: kill(2) with signal 0 performs only the existence and permission check.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(unix)]
fn kill_pid(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: kill(2) on a pid this host was given to manage; an error is ignored, as Go's
        // `Process.Kill` treats an already-exited process as success.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(not(unix))]
fn kill_pid(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_lines() {
        let (v, addr) = parse_protocol_line("1|1|unix|/tmp/plugin123|netrpc|", &[1]).unwrap();
        assert_eq!((v, addr), (1, PluginAddr::Unix("/tmp/plugin123".into())));
        let (_, addr) = parse_protocol_line("1|3|tcp|127.0.0.1:10000", &[2, 3]).unwrap();
        assert_eq!(addr, PluginAddr::Tcp("127.0.0.1:10000".parse().unwrap()));

        assert_eq!(
            parse_protocol_line("1|1|unix|/p|netrpc|", &[2])
                .unwrap_err()
                .to_string(),
            "incompatible API version with plugin. Plugin version: 1, Client versions: [2]"
        );
        assert_eq!(
            parse_protocol_line("1|1|unix|/p|netrpc|", &[2, 3])
                .unwrap_err()
                .to_string(),
            "incompatible API version with plugin. Plugin version: 1, Client versions: [2 3]"
        );
        assert!(matches!(
            parse_protocol_line("1|1|unix", &[1]),
            Err(ClientError::Unrecognized(_))
        ));
        assert!(matches!(
            parse_protocol_line("2|1|unix|/p", &[1]),
            Err(ClientError::CoreVersion(_))
        ));
        assert!(matches!(
            parse_protocol_line("1|1|pipe|/p", &[1]),
            Err(ClientError::UnknownAddress(_))
        ));
        assert!(matches!(
            parse_protocol_line("1|1|unix|/p|grpc|", &[1]),
            Err(ClientError::UnsupportedProtocol(_))
        ));
        let cert = "x".repeat(51);
        assert!(matches!(
            parse_protocol_line(&format!("1|1|unix|/p|netrpc|{cert}"), &[1]),
            Err(ClientError::AutoMtls)
        ));
    }
}
