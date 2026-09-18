//! The plugin side: serving a Rust plugin to a Mattermost host (client.go, `ClientMain`, and
//! client_rpc.go, `hooksRPCServer.OnActivate`).

use std::future::Future;
use std::sync::Arc;

use go_netrpc::{Server, ServiceError};
use goplugin::{HandshakeConfig, MuxBroker, ServeConfig, ServeError};

use super::file_upload::register_hooks_file_upload;
use super::serve_http::register_hooks_http;
use super::{ApiClient, Hooks, HooksFileUpload, HooksHttp, NotImplemented, hooks_server};
use crate::wire::plugin::{Z_OnActivateArgs, Z_OnActivateReturns, Z_OnConfigurationChangeArgs};

/// api.go, `handshake`: what a Mattermost host and plugin must agree on.
pub fn handshake() -> HandshakeConfig {
    HandshakeConfig {
        protocol_version: 1,
        magic_cookie_key: "MATTERMOST_PLUGIN".into(),
        magic_cookie_value: "Securely message teams, anywhere.".into(),
    }
}

/// A Mattermost plugin: its [`Hooks`], plus the activation steps that are not generated.
///
/// On activation the host names two brokered connections. The plugin then does what Go's
/// `hooksRPCServer.OnActivate` does, in the same order:
/// 1. It dials both connections and hands them to [`Plugin::set_api`].
/// 2. It calls [`Hooks::on_configuration_change`].
/// 3. It calls [`Plugin::on_activate`].
pub trait Plugin: Hooks + HooksHttp + HooksFileUpload {
    /// Go `MattermostPlugin.SetAPI` and `SetDriver`: the clients for this activation. `driver`
    /// is the raw net/rpc connection to the host's database driver, whose methods are not
    /// ported yet.
    fn set_api(&self, api: ApiClient, driver: go_netrpc::Client) {
        let _ = (api, driver);
    }

    /// Go: `OnActivate() error`. The default answers as Go does for a plugin without the hook:
    /// activation succeeds with no error.
    fn on_activate(
        &self,
    ) -> impl Future<Output = Result<Z_OnActivateReturns, NotImplemented>> + Send {
        async { Err(NotImplemented) }
    }
}

/// The net/rpc server for one dispense of the `"hooks"` plugin: [`hooks_server`], the HTTP hooks
/// and `Plugin.OnActivate`, all of which reach back over `broker`.
pub fn plugin_server<P: Plugin>(plugin: &Arc<P>, broker: MuxBroker) -> Server {
    let mut server = hooks_server(plugin);
    register_hooks_http(&mut server, plugin, &broker);
    register_hooks_file_upload(&mut server, plugin, &broker);
    let plugin = Arc::clone(plugin);
    server.register("Plugin.OnActivate", move |args: Z_OnActivateArgs| {
        let plugin = Arc::clone(&plugin);
        let broker = broker.clone();
        async move {
            // A failed dial fails the call, as Go returns the dial error.
            let api = broker
                .dial(args.api_mux_id)
                .await
                .map_err(|e| ServiceError(e.to_string()))?;
            let driver = broker
                .dial(args.driver_mux_id)
                .await
                .map_err(|e| ServiceError(e.to_string()))?;
            plugin.set_api(
                ApiClient::new(go_netrpc::Client::new(api), broker.clone()),
                go_netrpc::Client::new(driver),
            );

            // Go logs a configuration error to stderr and activates regardless.
            if let Ok(returns) = plugin
                .on_configuration_change(Z_OnConfigurationChangeArgs {})
                .await
                && let Some(err) = returns.a
            {
                eprint!(
                    "[ERROR] call to OnConfigurationChange failed, error: {}",
                    err.name
                );
            }

            Ok::<_, ServiceError>(plugin.on_activate().await.unwrap_or_default())
        }
    });
    server
}

/// Serve `plugin` to the Mattermost host that launched this process, until the host ends it
/// (client.go, `ClientMain`). Run by hand, it refuses with go-plugin's message.
pub async fn client_main<P: Plugin>(plugin: P) -> Result<(), ServeError> {
    let plugin = Arc::new(plugin);
    let config = ServeConfig::new(handshake()).plugin("hooks", move |broker: MuxBroker| {
        Ok(plugin_server(&plugin, broker))
    });
    goplugin::serve(config).await
}
