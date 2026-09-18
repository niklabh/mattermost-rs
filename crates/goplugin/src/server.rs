//! The plugin side: serving a plugin from its own process (server.go, `Serve`).

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;

use crate::client::{CORE_PROTOCOL_VERSION, HandshakeConfig};
use crate::rpc::{PluginServer, PluginStdio, serve_conn};
use crate::yamux;

/// How to serve (server.go, `ServeConfig`).
pub struct ServeConfig {
    pub handshake: HandshakeConfig,
    /// The plugins for `handshake.protocol_version`.
    pub plugins: HashMap<String, Arc<dyn PluginServer>>,
    /// Plugins for other protocol versions; the host's list picks one.
    pub versioned_plugins: BTreeMap<u32, HashMap<String, Arc<dyn PluginServer>>>,
    pub yamux: yamux::Config,
    /// Called with each connection's stdout and stderr streams.
    pub stdio: Arc<dyn Fn(PluginStdio) + Send + Sync>,
}

impl ServeConfig {
    pub fn new(handshake: HandshakeConfig) -> Self {
        Self {
            handshake,
            plugins: HashMap::new(),
            versioned_plugins: BTreeMap::new(),
            yamux: yamux::Config::default(),
            stdio: Arc::new(|_| {}),
        }
    }

    pub fn plugin(mut self, name: &str, plugin: impl PluginServer) -> Self {
        self.plugins.insert(name.to_owned(), Arc::new(plugin));
        self
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// No magic cookie configured: a bug in the plugin. Go exits with status 1.
    #[error("misconfigured ServeConfig: no magic cookie key or value")]
    Misconfigured,
    /// The magic cookie is not in the environment: someone ran the plugin by hand. Go exits with
    /// status 1.
    #[error("this binary is a plugin, not meant to be executed directly")]
    NotAPlugin,
    #[error("couldn't bind plugin TCP listener")]
    Bind,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// server.go, `protocolVersion`: the highest version both sides speak; if none, the lowest this
/// plugin speaks (Go's loop leaves it there), which the host will then refuse.
fn negotiate(
    config: &ServeConfig,
    client_versions: &str,
) -> (u32, HashMap<String, Arc<dyn PluginServer>>) {
    let mut sets = config.versioned_plugins.clone();
    if !config.plugins.is_empty() || sets.is_empty() {
        sets.insert(config.handshake.protocol_version, config.plugins.clone());
    }
    let client: Vec<u32> = client_versions
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let mut chosen = None;
    for (version, set) in sets.iter().rev() {
        chosen = Some((*version, set.clone()));
        if client.contains(version) {
            break;
        }
    }
    chosen.unwrap_or((config.handshake.protocol_version, HashMap::new()))
}

/// Serve until the host sends `Control.Quit`. On [`ServeError::NotAPlugin`] or
/// [`ServeError::Misconfigured`] the explanation has already been written to stderr, and the
/// caller should exit with status 1.
pub async fn serve(config: ServeConfig) -> Result<(), ServeError> {
    let hs = &config.handshake;
    if hs.magic_cookie_key.is_empty() || hs.magic_cookie_value.is_empty() {
        eprint!(
            "Misconfigured ServeConfig given to serve this plugin: no magic cookie\n\
             key or value was set. Please notify the plugin author and report\n\
             this as a bug.\n"
        );
        return Err(ServeError::Misconfigured);
    }
    if std::env::var(&hs.magic_cookie_key).ok().as_deref() != Some(hs.magic_cookie_value.as_str()) {
        eprint!(
            "This binary is a plugin. These are not meant to be executed directly.\n\
             Please execute the program that consumes these plugins, which will\n\
             load any plugins automatically\n"
        );
        return Err(ServeError::NotAPlugin);
    }
    let (version, plugins) = negotiate(
        &config,
        &std::env::var("PLUGIN_PROTOCOL_VERSIONS").unwrap_or_default(),
    );
    let plugins = Arc::new(plugins);
    let (done_tx, mut done_rx) = watch::channel(false);

    let listener = Listener::bind().await?;
    let line = format!(
        "{CORE_PROTOCOL_VERSION}|{version}|{}|{}|netrpc|\n",
        listener.network(),
        listener.address()
    );
    {
        let mut out = std::io::stdout().lock();
        out.write_all(line.as_bytes())?;
        out.flush()?;
    }

    // server.go: interrupts are ignored; the host decides when a plugin stops.
    tokio::spawn(async {
        while tokio::signal::ctrl_c().await.is_ok() {
            tracing::trace!("plugin received interrupt signal, ignoring");
        }
    });

    let accept = async {
        loop {
            let conn = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "plugin: plugin server");
                    return;
                }
            };
            let plugins = plugins.clone();
            let done = done_tx.clone();
            let stdio = config.stdio.clone();
            let yamux_config = config.yamux.clone();
            tokio::spawn(async move {
                let stdio = move |s| stdio(s);
                if let Err(e) = conn.serve(yamux_config, plugins, done, stdio).await {
                    tracing::debug!(error = %e, "go-plugin: connection ended");
                }
            });
        }
    };
    tokio::select! {
        () = accept => {}
        _ = done_rx.wait_for(|d| *d) => {}
    }
    listener.cleanup();
    Ok(())
}

enum Listener {
    #[cfg(unix)]
    Unix(tokio::net::UnixListener, PathBuf),
    /// go-plugin listens on TCP only where it has no unix sockets (Windows).
    #[cfg_attr(unix, allow(dead_code))]
    Tcp(tokio::net::TcpListener),
}

enum Conn {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg_attr(unix, allow(dead_code))]
    Tcp(tokio::net::TcpStream),
}

impl Conn {
    async fn serve(
        self,
        config: yamux::Config,
        plugins: Arc<HashMap<String, Arc<dyn PluginServer>>>,
        done: watch::Sender<bool>,
        stdio: impl FnOnce(PluginStdio) + Send,
    ) -> Result<(), crate::rpc::RpcError> {
        match self {
            #[cfg(unix)]
            Conn::Unix(s) => serve_conn(s, config, plugins, done, stdio).await,
            Conn::Tcp(s) => serve_conn(s, config, plugins, done, stdio).await,
        }
    }
}

impl Listener {
    #[cfg(unix)]
    async fn bind() -> Result<Self, ServeError> {
        // server.go, serverListener_unix: a fresh name from os.CreateTemp(dir, "plugin").
        let dir = std::env::var_os("PLUGIN_UNIX_SOCKET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = loop {
            let candidate = dir.join(format!("plugin{}", random_suffix()));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(_) => {
                    std::fs::remove_file(&candidate)?;
                    break candidate;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        let listener = tokio::net::UnixListener::bind(&path)?;
        if let Ok(group) = std::env::var("PLUGIN_UNIX_SOCKET_GROUP")
            && !group.is_empty()
        {
            set_group_writable(&path, &group)?;
        }
        Ok(Listener::Unix(listener, path))
    }

    #[cfg(not(unix))]
    async fn bind() -> Result<Self, ServeError> {
        // server.go, serverListener_tcp: the first free port in [PLUGIN_MIN_PORT, PLUGIN_MAX_PORT].
        let port = |k: &str| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(0)
        };
        let (min, max) = (port("PLUGIN_MIN_PORT"), port("PLUGIN_MAX_PORT"));
        for p in min..=max {
            if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", p)).await {
                return Ok(Listener::Tcp(l));
            }
        }
        Err(ServeError::Bind)
    }

    fn network(&self) -> &'static str {
        match self {
            #[cfg(unix)]
            Listener::Unix(..) => "unix",
            Listener::Tcp(_) => "tcp",
        }
    }

    fn address(&self) -> String {
        match self {
            #[cfg(unix)]
            Listener::Unix(_, path) => path.display().to_string(),
            Listener::Tcp(l) => l.local_addr().map(|a| a.to_string()).unwrap_or_default(),
        }
    }

    async fn accept(&self) -> std::io::Result<Conn> {
        Ok(match self {
            #[cfg(unix)]
            Listener::Unix(l, _) => Conn::Unix(l.accept().await?.0),
            Listener::Tcp(l) => Conn::Tcp(l.accept().await?.0),
        })
    }

    /// server.go's `rmListener`: the socket file goes with the listener.
    fn cleanup(&self) {
        #[cfg(unix)]
        if let Listener::Unix(_, path) = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
fn random_suffix() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    h.finish() as u32
}

/// server.go, `setGroupWritable`: the group by gid or name, mode 0660.
#[cfg(unix)]
fn set_group_writable(path: &std::path::Path, group: &str) -> Result<(), ServeError> {
    use std::os::unix::ffi::OsStrExt;
    let gid: libc::gid_t = match group.parse() {
        Ok(gid) => gid,
        Err(_) => {
            let name = std::ffi::CString::new(group)
                .map_err(|_| std::io::Error::other("bad group name"))?;
            // SAFETY: getgrnam reads a NUL-terminated name and returns a pointer into static
            // storage, read once here before any other call could overwrite it.
            let entry = unsafe { libc::getgrnam(name.as_ptr()) };
            if entry.is_null() {
                return Err(
                    std::io::Error::other(format!("failed to find gid from {group:?}")).into(),
                );
            }
            // SAFETY: non-null, checked above.
            unsafe { (*entry).gr_gid }
        }
    };
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("bad path"))?;
    // SAFETY: chown(2) on a path this process just created; `-1` leaves the owner unchanged.
    if unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o660))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use go_netrpc::Server;

    fn config(versions: &[u32]) -> ServeConfig {
        let mut c = ServeConfig::new(HandshakeConfig {
            protocol_version: versions[0],
            magic_cookie_key: "K".into(),
            magic_cookie_value: "V".into(),
        });
        for v in versions {
            let mut set: HashMap<String, Arc<dyn PluginServer>> = HashMap::new();
            let name = format!("v{v}");
            set.insert(
                name,
                Arc::new(|_: crate::MuxBroker| Ok::<_, String>(Server::new())),
            );
            c.versioned_plugins.insert(*v, set);
        }
        c
    }

    /// server.go, `protocolVersion`: versions are tried highest first, and the loop leaves the
    /// lowest one chosen when none matches.
    #[test]
    fn negotiation() {
        let c = config(&[1, 3, 5]);
        let pick = |client: &str| {
            let (v, set) = negotiate(&c, client);
            (v, set.keys().next().cloned().unwrap_or_default())
        };
        assert_eq!(pick("3"), (3, "v3".into()));
        assert_eq!(pick("1,3"), (3, "v3".into()), "the highest common version");
        assert_eq!(pick("5,1"), (5, "v5".into()));
        assert_eq!(
            pick("2,4"),
            (1, "v1".into()),
            "no match: the lowest, which the host refuses"
        );
        assert_eq!(pick(""), (1, "v1".into()));
        assert_eq!(
            pick("x,3"),
            (3, "v3".into()),
            "unparsable entries are skipped"
        );
    }
}
