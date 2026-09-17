//! go-plugin's net/rpc protocol on one connection (rpc_client.go, rpc_server.go).
//!
//! The host is the yamux client. It opens three streams, in this order and with their SYNs sent
//! at once: the control stream (net/rpc services `Control` and `Dispenser`), then the plugin's
//! stdout and stderr. Every other stream goes through the [`MuxBroker`]. Dispensing a plugin asks
//! the plugin to serve that plugin's `Plugin.*` methods on a brokered stream, which the host then
//! dials.

use std::collections::HashMap;
use std::sync::Arc;

use go_netrpc::{Server, ServiceError};
use gobwire::Gob;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;

use crate::broker::{BrokerError, MuxBroker};
use crate::yamux::{self, Session, Stream};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RpcError {
    #[error(transparent)]
    Yamux(#[from] yamux::Error),
    #[error(transparent)]
    Broker(#[from] BrokerError),
    #[error(transparent)]
    NetRpc(#[from] go_netrpc::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// `struct{}` as net/rpc sends it.
#[derive(Gob, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Empty {}

/// A dispensed plugin: a net/rpc client for its `Plugin.*` methods, and the broker for further
/// streams (what go-plugin hands `Plugin.Client`).
#[derive(Clone)]
pub struct Dispensed {
    pub client: go_netrpc::Client,
    pub broker: MuxBroker,
}

/// The host's side of a plugin connection (rpc_client.go, `RPCClient`).
#[derive(Clone)]
pub struct RpcClient {
    session: Session,
    broker: MuxBroker,
    control: go_netrpc::Client,
}

impl RpcClient {
    /// Start the protocol on a connected transport. `stdout` and `stderr` receive what the plugin
    /// writes to its standard streams (rpc_client.go, `SyncStreams`).
    pub async fn connect<T, O, E>(
        io: T,
        config: yamux::Config,
        stdout: O,
        stderr: E,
    ) -> Result<Self, RpcError>
    where
        T: AsyncRead + AsyncWrite + Send + 'static,
        O: AsyncWrite + Send + Unpin + 'static,
        E: AsyncWrite + Send + Unpin + 'static,
    {
        let session = Session::client(io, config)?;
        let control = session.open().await?;
        let out = session.open().await?;
        let err = session.open().await?;
        tokio::spawn(copy_stream(out, stdout));
        tokio::spawn(copy_stream(err, stderr));
        let (broker, run) = MuxBroker::new(session.clone());
        tokio::spawn(run);
        Ok(Self {
            session,
            broker,
            control: go_netrpc::Client::new(control),
        })
    }

    pub fn broker(&self) -> &MuxBroker {
        &self.broker
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Ask the plugin for `name` and connect to it (rpc_client.go, `Dispense`).
    pub async fn dispense(&self, name: &str) -> Result<Dispensed, RpcError> {
        let id: u32 = self.control.call("Dispenser.Dispense", name).await?;
        let stream = self.broker.dial(id).await?;
        Ok(Dispensed {
            client: go_netrpc::Client::new(stream),
            broker: self.broker.clone(),
        })
    }

    /// `Control.Ping`: whether the plugin still answers.
    pub async fn ping(&self) -> Result<(), RpcError> {
        let _: Empty = self.control.call("Control.Ping", &true).await?;
        Ok(())
    }

    /// Ask the plugin to exit (`Control.Quit`), then close the connection. Returns the Quit
    /// call's error, which go-plugin uses to decide whether to kill the process instead.
    pub async fn close(&self) -> Result<(), RpcError> {
        let quit = self.control.call::<_, Empty>("Control.Quit", &true).await;
        let _ = self.control.close().await;
        self.session.close();
        quit.map(|_| ()).map_err(Into::into)
    }
}

async fn copy_stream<W: AsyncWrite + Unpin>(mut from: Stream, mut to: W) {
    if let Err(e) = tokio::io::copy(&mut from, &mut to).await {
        tracing::error!(error = %e, "plugin: stream copy error");
    }
    let _ = to.flush().await;
}

/// A plugin type the plugin process can dispense: builds the net/rpc server for one connection
/// (go-plugin's `Plugin.Server`). Register its methods as `"Plugin.<Method>"`.
pub trait PluginServer: Send + Sync + 'static {
    fn server(&self, broker: MuxBroker) -> Result<Server, String>;
}

impl<F> PluginServer for F
where
    F: Fn(MuxBroker) -> Result<Server, String> + Send + Sync + 'static,
{
    fn server(&self, broker: MuxBroker) -> Result<Server, String> {
        self(broker)
    }
}

/// The plugin's standard streams on one connection, for a plugin to write to. (go-plugin swaps
/// `os.Stdout` and `os.Stderr` for these; a Rust process cannot, so they are handed over.)
pub struct PluginStdio {
    pub stdout: Stream,
    pub stderr: Stream,
}

/// Serve one connection from a host (rpc_server.go, `ServeConn`). Returns when the host closes
/// the connection; `done` is set when the host sends `Control.Quit`.
pub async fn serve_conn<T>(
    io: T,
    config: yamux::Config,
    plugins: Arc<HashMap<String, Arc<dyn PluginServer>>>,
    done: watch::Sender<bool>,
    stdio: impl FnOnce(PluginStdio) + Send,
) -> Result<(), RpcError>
where
    T: AsyncRead + AsyncWrite + Send + 'static,
{
    let session = Session::server(io, config)?;
    let control = session.accept().await?;
    let stdout = session.accept().await?;
    let stderr = session.accept().await?;
    stdio(PluginStdio { stdout, stderr });
    let (broker, run) = MuxBroker::new(session.clone());
    tokio::spawn(run);

    let mut server = Server::new();
    server.register("Control.Ping", |_: bool| async {
        Ok::<_, ServiceError>(Empty {})
    });
    let quit = done.clone();
    server.register("Control.Quit", move |_: bool| {
        let quit = quit.clone();
        async move {
            quit.send_replace(true);
            Ok::<_, ServiceError>(Empty {})
        }
    });
    let dispense_broker = broker.clone();
    server.register("Dispenser.Dispense", move |name: String| {
        let broker = dispense_broker.clone();
        let plugins = plugins.clone();
        async move {
            // rpc_server.go, dispenseServer.Dispense.
            let plugin = plugins
                .get(&name)
                .cloned()
                .ok_or_else(|| ServiceError(format!("unknown plugin type: {name}")))?;
            let server = plugin.server(broker.clone()).map_err(ServiceError)?;
            let id = broker.next_id();
            let server = Arc::new(server);
            tokio::spawn(async move {
                match broker.accept(id).await {
                    Ok(stream) => {
                        let _ = server.serve(stream).await;
                    }
                    Err(e) => tracing::error!(name, error = %e, "go-plugin: plugin dispense error"),
                }
            });
            Ok::<_, ServiceError>(id)
        }
    });
    Arc::new(server).serve(control).await?;
    session.close();
    Ok(())
}
