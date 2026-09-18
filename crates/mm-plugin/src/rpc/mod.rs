//! The plugin RPC in both directions (client_rpc.go, client_rpc_generated.go).
//!
//! The host drives a plugin's hooks through [`HooksClient`] and serves the server API to it with
//! [`register_api`] over a [`PluginApi`] implementation. A plugin serves its hooks with
//! [`register_hooks`] over a [`Hooks`] implementation and calls the API through [`ApiClient`].
//!
//! Arguments and returns are the generated wire structs (`Z_<Method>Args`, `Z_<Method>Returns`),
//! field for field what Go sends, so nothing is converted on the way through. A plugin process
//! runs [`client_main`]. The methods in `excludedPluginHooks` are not here yet, apart from
//! `Implemented` and both halves of `OnActivate`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use go_netrpc::Server;
use gobwire::{Decode, Encode};
use goplugin::rpc::Empty;
use goplugin::{Dispensed, MuxBroker};

use crate::wire::plugin::{Z_OnActivateArgs, Z_OnActivateReturns};

mod api;
mod driver;
mod file_upload;
mod handwritten;
mod hooks;
mod plugin;
mod serve_http;
mod streams;

pub use api::{PluginApi, register_api as register_generated_api};
pub use driver::{Driver, register_driver};
pub use file_upload::HooksFileUpload;
pub use hooks::{HOOK_NAMES, Hooks, hook_id, register_hooks};
pub use plugin::{Plugin, client_main, handshake, plugin_server};
pub use serve_http::HooksHttp;
pub use streams::{HttpResponse, PluginApiHttp, PluginApiStreams, RemoteHttpResponse};

/// A [`Hooks`] or [`PluginApi`] method the implementation does not provide.
///
/// The server answers the call with Go's error for a method the implementation lacks: the call
/// fails with `Hook <Name> called but not implemented.` or `API <Name> called but not
/// implemented.`, and the caller receives zero values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotImplemented;

/// Every method of [`PluginApi`] a plugin can call: the generated ones, `LoadPluginConfiguration`
/// and the methods that carry a reader, whose servers are hand-written. The broker is how those
/// readers reach the plugin.
pub fn register_api<T: PluginApi + PluginApiStreams + PluginApiHttp>(
    server: &mut Server,
    implementation: &Arc<T>,
    broker: &MuxBroker,
) {
    register_generated_api(server, implementation);
    handwritten::register_load_plugin_configuration(server, implementation);
    streams::register_api_streams(server, implementation, broker);
    streams::register_api_http(server, implementation, broker);
}

/// `Plugin.Implemented` plus every generated hook (client_rpc.go, `hooksRPCServer`). A plugin
/// process serves [`plugin_server`], which adds `OnActivate`.
pub fn hooks_server<H: Hooks>(hooks: &Arc<H>) -> Server {
    let mut server = Server::new();
    let this = Arc::clone(hooks);
    // client_rpc.go, `hooksRPCServer.Implemented`, which never fails.
    server.register("Plugin.Implemented", move |_: Empty| {
        let names = this.implemented();
        async move { Ok::<_, go_netrpc::ServiceError>(names) }
    });
    register_hooks(&mut server, hooks);
    server
}

/// The host's handle on one plugin's hooks (client_rpc.go, `hooksRPCClient`).
pub struct HooksClient {
    client: go_netrpc::Client,
    broker: MuxBroker,
    implemented: [AtomicBool; hook_id::TOTAL_HOOKS],
}

impl HooksClient {
    /// Wrap the connection `goplugin` dispensed for the `"hooks"` plugin. No hook counts as
    /// implemented until [`HooksClient::implemented`] has asked.
    pub fn new(dispensed: Dispensed) -> Self {
        Self {
            client: dispensed.client,
            broker: dispensed.broker,
            implemented: std::array::from_fn(|_| AtomicBool::new(false)),
        }
    }

    /// `Plugin.Implemented`: ask which hooks the plugin implements, and remember the ones with a
    /// hook id. Names without one are returned but change nothing, as in Go.
    pub async fn implemented(&self) -> Result<Vec<String>, go_netrpc::Error> {
        let names: Vec<String> = self.client.call("Plugin.Implemented", &Empty {}).await?;
        for name in &names {
            if let Some(id) = HOOK_NAMES.iter().position(|n| n == name) {
                self.implemented[id].store(true, Ordering::Relaxed);
            }
        }
        Ok(names)
    }

    /// Whether the plugin reported hook `id` as implemented (supervisor.go, `Implements`).
    pub fn implements(&self, id: usize) -> bool {
        self.implemented
            .get(id)
            .is_some_and(|i| i.load(Ordering::Relaxed))
    }

    /// The host half of `OnActivate` (client_rpc.go): serve the API and the database driver on
    /// two brokered connections, then tell the plugin their ids. Unlike every other hook it is
    /// called whether or not the plugin implements it, because the plugin's API client is set up
    /// here.
    ///
    /// `driver` answers the plugin's database calls on the second connection (db_rpc.go).
    pub async fn on_activate<A, D>(&self, api: &Arc<A>, driver: &Arc<D>) -> Z_OnActivateReturns
    where
        A: PluginApi + PluginApiStreams + PluginApiHttp,
        D: Driver,
    {
        let mut api_server = Server::new();
        register_api(&mut api_server, api, &self.broker);
        let api_mux_id = self.broker.next_id();
        let broker = self.broker.clone();
        tokio::spawn(async move {
            broker
                .accept_and_serve(api_mux_id, Arc::new(api_server))
                .await;
        });

        let mut driver_server = Server::new();
        register_driver(&mut driver_server, driver);
        let driver_mux_id = self.broker.next_id();
        let broker = self.broker.clone();
        tokio::spawn(async move {
            broker
                .accept_and_serve(driver_mux_id, Arc::new(driver_server))
                .await;
        });

        let args = Z_OnActivateArgs {
            api_mux_id,
            driver_mux_id,
        };
        match self.client.call("Plugin.OnActivate", &args).await {
            Ok(returns) => returns,
            Err(e) => {
                tracing::error!(error = %e, "RPC call to OnActivate plugin failed.");
                Z_OnActivateReturns::default()
            }
        }
    }

    async fn call<A, R>(&self, id: usize, name: &str, args: &A) -> R
    where
        A: Encode,
        R: Decode + Default + Send + 'static,
    {
        if !self.implements(id) {
            return R::default();
        }
        match self.rpc(name, args).await {
            Ok(returns) => returns,
            Err(e) => {
                tracing::error!(error = %e, "RPC call {name} to plugin failed.");
                R::default()
            }
        }
    }

    /// [`HooksClient::call`] for the hooks whose answer defaults to `seed`: the reply decodes
    /// **into** it, so a field the plugin leaves out keeps the caller's value.
    async fn call_merging<A, R>(&self, id: usize, name: &str, args: &A, seed: R) -> R
    where
        A: Encode,
        R: Decode + Clone + Default + Send + 'static,
    {
        if !self.implements(id) {
            return seed;
        }
        match self
            .client
            .call_into(&format!("Plugin.{name}"), args, seed.clone())
            .await
        {
            Ok(returns) => returns,
            Err(e) => {
                tracing::error!(error = %e, "RPC call {name} to plugin failed.");
                // Go answers with its seeded struct, which a failed call left untouched.
                seed
            }
        }
    }

    async fn rpc<A, R>(&self, name: &str, args: &A) -> Result<R, go_netrpc::Error>
    where
        A: Encode,
        R: Decode + Default + Send + 'static,
    {
        self.client.call(&format!("Plugin.{name}"), args).await
    }

    /// client_rpc_generated.go, `<Hook>WithRPCErr`: the returns are zero values whenever the
    /// transport error is set.
    async fn call_with_rpc_err<A, R>(
        &self,
        id: usize,
        name: &str,
        args: &A,
    ) -> (R, Option<go_netrpc::Error>)
    where
        A: Encode,
        R: Decode + Default + Send + 'static,
    {
        if !self.implements(id) {
            return (R::default(), None);
        }
        match self.client.call(&format!("Plugin.{name}"), args).await {
            Ok(returns) => (returns, None),
            Err(e) => {
                tracing::debug!(error = %e, "RPC call {name} to plugin failed.");
                (R::default(), Some(e))
            }
        }
    }
}

/// The plugin's handle on the host's database (db_rpc.go, `dbRPCClient`).
///
/// Every method answers zero values when the call fails, as Go's does, and the error fields come
/// back as they were sent: read them with [`crate::error::decodable_error`].
#[derive(Clone)]
pub struct DriverClient {
    client: go_netrpc::Client,
}

impl DriverClient {
    /// A client over the brokered connection the host named in `OnActivate`.
    pub fn new(client: go_netrpc::Client) -> Self {
        Self { client }
    }

    async fn call<A, R>(&self, name: &str, args: &A) -> R
    where
        A: Encode,
        R: Decode + Default + Send + 'static,
    {
        match self.client.call(&format!("Plugin.{name}"), args).await {
            Ok(returns) => returns,
            Err(e) => {
                tracing::error!(error = %e, "error during Plugin.{name}");
                R::default()
            }
        }
    }
}

/// The plugin's handle on the server API (client_rpc.go, `apiRPCClient`).
#[derive(Clone)]
pub struct ApiClient {
    client: go_netrpc::Client,
    /// The plugin's broker, for the methods that lend the host a reader.
    broker: MuxBroker,
}

impl ApiClient {
    /// A client over the brokered connection the host named in `OnActivate`.
    pub fn new(client: go_netrpc::Client, broker: MuxBroker) -> Self {
        Self { client, broker }
    }

    /// The net/rpc client underneath, for calling a method this crate does not wrap.
    pub fn client(&self) -> &go_netrpc::Client {
        &self.client
    }

    /// The plugin's broker, for lending the host a stream this crate does not wrap.
    pub fn broker(&self) -> &MuxBroker {
        &self.broker
    }

    async fn call<A, R>(&self, name: &str, args: &A) -> R
    where
        A: Encode,
        R: Decode + Default + Send + 'static,
    {
        match self.client.call(&format!("Plugin.{name}"), args).await {
            Ok(returns) => returns,
            Err(e) => {
                tracing::error!(error = %e, "RPC call to {name} API failed");
                R::default()
            }
        }
    }
}
