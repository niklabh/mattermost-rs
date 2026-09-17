//! The generated RPC layer against Go, and against itself.
//!
//! **Rust host, Go plugin.** `reference/dump/plugingen plugin` is a real plugin served by
//! `plugin.ClientMain`. Its hooks are plugintest's mock, and each answers with the oracle's
//! `Z_<Hook>Returns`. Its `OnActivate` calls every generated API method with the oracle's
//! `Z_<Method>Args`. The Rust host launches it through `goplugin` and serves the API from a fake
//! that answers with `Z_<Method>Returns` and records what it received. It calls every generated
//! hook through `HooksClient` with `Z_<Hook>Args`. Then both sides are compared with the oracle:
//! - what Go received as hook arguments, and what Rust received as API arguments;
//! - what Rust received as hook returns: the fixture after Go's `encodableError`, as the plugin
//!   recorded it;
//! - what Go received as API returns.
//!
//! **Rust with Rust.** `register_hooks` behind `HooksClient`, and `register_api` behind
//! `ApiClient`, round-trip every method. A plugin that implements one hook is skipped for every
//! other, and a hook it claims but does not provide fails with Go's not-implemented error.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use go_netrpc::Server;
use goplugin::yamux::{Config, Session};
use goplugin::{Client, ClientConfig, Dispensed, HandshakeConfig, MuxBroker, PluginCommand};
use mm_plugin::rpc::{
    ApiClient, Hooks, HooksClient, NotImplemented, PluginApi, hooks_server, register_api,
};
use serde_json::{Map, Value as Json};

mod common;
use common::*;

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(120), f)
        .await
        .expect("test timed out")
}

/// Answers every generated method with its `Z_<Method>Returns` fixture and records how the
/// arguments it received render.
#[derive(Default)]
struct Fake {
    received: Mutex<BTreeMap<String, Json>>,
    implemented: Vec<String>,
}

impl Fake {
    fn answer<A: gobwire::Encode, R: gobwire::Decode + Default + Send + 'static>(
        &self,
        name: &str,
        args: &A,
    ) -> R {
        self.received
            .lock()
            .unwrap()
            .insert(name.to_owned(), render_typed(args));
        fixture(&format!("Z_{name}Returns"))
    }

    fn received(&self) -> BTreeMap<String, Json> {
        self.received.lock().unwrap().clone()
    }
}

macro_rules! fake_api {
    ($(($method:ident, $name:literal, $args:ty, $returns:ty),)*) => {
        impl PluginApi for Fake {
            $(
                async fn $method(&self, args: $args) -> Result<$returns, NotImplemented> {
                    Ok(self.answer($name, &args))
                }
            )*
        }
    };
}
mm_plugin::for_each_api_method!(fake_api);

macro_rules! fake_hooks {
    ($(($method:ident, $name:literal, $args:ty, $returns:ty),)*) => {
        impl Hooks for Fake {
            fn implemented(&self) -> Vec<String> {
                self.implemented.clone()
            }
            $(
                async fn $method(&self, args: $args) -> Result<$returns, NotImplemented> {
                    Ok(self.answer($name, &args))
                }
            )*
        }
    };
}
mm_plugin::for_each_hook!(fake_hooks);

macro_rules! names {
    ($(($method:ident, $name:literal, $args:ty, $returns:ty),)*) => {
        &[$($name),*]
    };
}
const HOOKS: &[&str] = mm_plugin::for_each_hook!(names);
const API: &[&str] = mm_plugin::for_each_api_method!(names);

/// Call every generated hook with its `Z_<Hook>Args` fixture; the rendered returns by name.
async fn call_every_hook(client: &HooksClient) -> BTreeMap<String, Json> {
    let mut out = BTreeMap::new();
    macro_rules! call {
        ($(($method:ident, $name:literal, $args:ty, $returns:ty),)*) => {
            $(
                let returns = client.$method(fixture::<$args>(concat!("Z_", $name, "Args"))).await;
                out.insert($name.to_owned(), render_typed(&returns));
            )*
        };
    }
    mm_plugin::for_each_hook!(call);
    out
}

/// Call every generated API method with its `Z_<Method>Args` fixture; the rendered returns.
async fn call_every_api_method(client: &ApiClient) -> BTreeMap<String, Json> {
    let mut out = BTreeMap::new();
    macro_rules! call {
        ($(($method:ident, $name:literal, $args:ty, $returns:ty),)*) => {
            $(
                let returns = client.$method(fixture::<$args>(concat!("Z_", $name, "Args"))).await;
                out.insert($name.to_owned(), render_typed(&returns));
            )*
        };
    }
    mm_plugin::for_each_api_method!(call);
    out
}

fn handshake() -> HandshakeConfig {
    // api.go, `handshake`.
    HandshakeConfig {
        protocol_version: 1,
        magic_cookie_key: "MATTERMOST_PLUGIN".into(),
        magic_cookie_value: "Securely message teams, anywhere.".into(),
    }
}

fn read_transcript(path: &PathBuf) -> Vec<Map<String, Json>> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn rpc_rust_host_drives_the_go_plugin() {
    let expected = expected();
    let dir = scratch("rpc-go-plugin");
    let transcript = dir.join("transcript.jsonl");

    let mut cmd = PluginCommand::new(plugingen())
        .arg("plugin")
        .arg(fixtures().join("gob"));
    cmd.env
        .push(("PLUGINGEN_TRANSCRIPT".into(), transcript.clone().into()));
    cmd.current_dir = Some(root().join("reference/dump"));
    let mut config = ClientConfig::new(handshake());
    config.cmd = Some(cmd);
    let plugin = within(Client::start(config)).await.unwrap();
    let hooks = HooksClient::new(within(plugin.dispense("hooks")).await.unwrap());

    let implemented = within(hooks.implemented()).await.unwrap();
    for name in HOOKS {
        assert!(
            implemented.iter().any(|n| n == name),
            "{name} not implemented"
        );
    }

    let api = Arc::new(Fake::default());
    let activated = within(hooks.on_activate(&api)).await;
    assert_eq!(activated.a, None, "OnActivate returned an error");
    let returned = within(call_every_hook(&hooks)).await;
    within(plugin.kill()).await;

    let mut failures = Vec::new();
    let mut go_hooks = BTreeMap::new();
    let mut go_api = BTreeMap::new();
    let mut activated = false;
    for entry in read_transcript(&transcript) {
        if let Some(Json::String(name)) = entry.get("hook") {
            // A later call replaces an earlier one: OnActivate itself calls OnConfigurationChange.
            go_hooks.insert(name.clone(), entry);
        } else if let Some(Json::String(name)) = entry.get("api") {
            go_api.insert(name.clone(), entry["returns"].clone());
        } else if entry.get("activated").is_some() {
            activated = true;
        }
    }
    assert!(
        activated,
        "the Go plugin's OnActivate did not finish its API tour"
    );

    for name in HOOKS {
        let Some(go) = go_hooks.get(*name) else {
            failures.push(format!("hook {name}: the Go plugin never saw the call"));
            continue;
        };
        let want_args = &expected[&format!("Z_{name}Args")];
        if &go["args"] != want_args {
            failures.push(format!(
                "hook {name}: Go received\n{}\nexpected\n{want_args}",
                go["args"]
            ));
        }
        if returned[*name] != go["returns"] {
            failures.push(format!(
                "hook {name}: Rust received\n{}\nGo sent\n{}",
                returned[*name], go["returns"]
            ));
        }
    }
    let received = api.received();
    for name in API {
        let want_args = &expected[&format!("Z_{name}Args")];
        match received.get(*name) {
            Some(got) if got == want_args => {}
            Some(got) => failures.push(format!(
                "api {name}: Rust received\n{got}\nexpected\n{want_args}"
            )),
            None => failures.push(format!("api {name}: the Rust server never saw the call")),
        }
        let want_returns = &expected[&format!("Z_{name}Returns")];
        match go_api.get(*name) {
            Some(got) if got == want_returns => {}
            Some(got) => failures.push(format!(
                "api {name}: Go received\n{got}\nexpected\n{want_returns}"
            )),
            None => failures.push(format!("api {name}: the Go plugin recorded no returns")),
        }
    }
    report(&failures);
}

/// A connected host and plugin over an in-memory yamux session, with the plugin serving `hooks`.
/// As in go-plugin, each side's broker starts only after the hooks stream is open: a running broker
/// claims every stream the session accepts.
async fn rust_pair<H: Hooks>(hooks: Arc<H>) -> HooksClient {
    let (a, b) = tokio::io::duplex(1 << 20);
    let host = Session::client(a, Config::default()).unwrap();
    let plugin = Session::server(b, Config::default()).unwrap();

    let server = hooks_server(&hooks);
    tokio::spawn(async move {
        let stream = plugin.accept().await.unwrap();
        let (broker, run) = MuxBroker::new(plugin.clone());
        tokio::spawn(run);
        let _ = Arc::new(server).serve(stream).await;
        // The session lives as long as the connection is served.
        drop((plugin, broker));
    });
    let stream = host.open().await.unwrap();
    let (host_broker, run) = MuxBroker::new(host.clone());
    tokio::spawn(run);
    HooksClient::new(Dispensed {
        client: go_netrpc::Client::new(stream),
        broker: host_broker,
    })
}

#[tokio::test]
async fn rpc_rust_hooks_round_trip_every_hook() {
    let expected = expected();
    let fake = Arc::new(Fake {
        implemented: HOOKS.iter().map(|s| (*s).to_owned()).collect(),
        ..Fake::default()
    });
    let client = rust_pair(Arc::clone(&fake)).await;
    assert_eq!(
        within(client.implemented()).await.unwrap().len(),
        HOOKS.len()
    );
    let returned = within(call_every_hook(&client)).await;

    let mut failures = Vec::new();
    let received = fake.received();
    for name in HOOKS {
        let args = &expected[&format!("Z_{name}Args")];
        if received.get(*name) != Some(args) {
            failures.push(format!(
                "hook {name}: the plugin received {:?}",
                received.get(*name)
            ));
        }
        let returns = &expected[&format!("Z_{name}Returns")];
        if &returned[*name] != returns {
            failures.push(format!(
                "hook {name}: the host received\n{}\nexpected\n{returns}",
                returned[*name]
            ));
        }
    }
    report(&failures);
}

#[tokio::test]
async fn rpc_rust_api_round_trips_every_method() {
    let expected = expected();
    let fake = Arc::new(Fake::default());
    let (a, b) = tokio::io::duplex(1 << 20);
    let mut server = Server::new();
    register_api(&mut server, &fake);
    tokio::spawn(Arc::new(server).serve(b));
    let client = ApiClient::new(go_netrpc::Client::new(a));
    let returned = within(call_every_api_method(&client)).await;

    let mut failures = Vec::new();
    let received = fake.received();
    for name in API {
        let args = &expected[&format!("Z_{name}Args")];
        if received.get(*name) != Some(args) {
            failures.push(format!(
                "api {name}: the host received {:?}",
                received.get(*name)
            ));
        }
        let returns = &expected[&format!("Z_{name}Returns")];
        if &returned[*name] != returns {
            failures.push(format!(
                "api {name}: the plugin received\n{}\nexpected\n{returns}",
                returned[*name]
            ));
        }
    }
    report(&failures);
}

/// Implements `OnDeactivate` and nothing else, but claims `OnInstall` too.
struct OneHook {
    calls: Mutex<Vec<&'static str>>,
}

impl Hooks for OneHook {
    fn implemented(&self) -> Vec<String> {
        vec!["OnDeactivate".into(), "OnInstall".into(), "NotAHook".into()]
    }

    async fn on_deactivate(
        &self,
        _: mm_plugin::wire::plugin::Z_OnDeactivateArgs,
    ) -> Result<mm_plugin::wire::plugin::Z_OnDeactivateReturns, NotImplemented> {
        self.calls.lock().unwrap().push("OnDeactivate");
        Ok(fixture("Z_OnDeactivateReturns"))
    }

    async fn user_has_been_created(
        &self,
        _: mm_plugin::wire::plugin::Z_UserHasBeenCreatedArgs,
    ) -> Result<mm_plugin::wire::plugin::Z_UserHasBeenCreatedReturns, NotImplemented> {
        self.calls.lock().unwrap().push("UserHasBeenCreated");
        Ok(Default::default())
    }
}

#[tokio::test]
async fn rpc_unimplemented_hooks_are_skipped_and_unprovided_ones_fail_as_in_go() {
    use mm_plugin::rpc::hook_id;
    use mm_plugin::wire::plugin::{Z_OnDeactivateArgs, Z_OnInstallArgs, Z_UserHasBeenCreatedArgs};

    let plugin = Arc::new(OneHook {
        calls: Mutex::new(Vec::new()),
    });
    let client = rust_pair(Arc::clone(&plugin)).await;

    // Nothing counts as implemented before the plugin is asked.
    assert!(!client.implements(hook_id::ON_DEACTIVATE));
    assert_eq!(
        within(client.on_deactivate(Z_OnDeactivateArgs {})).await,
        Default::default()
    );
    assert!(plugin.calls.lock().unwrap().is_empty());

    // A name without a hook id is reported but marks nothing.
    let names = within(client.implemented()).await.unwrap();
    assert_eq!(names, ["OnDeactivate", "OnInstall", "NotAHook"]);
    assert!(client.implements(hook_id::ON_DEACTIVATE));
    assert!(client.implements(hook_id::ON_INSTALL));
    assert!(!client.implements(hook_id::USER_HAS_BEEN_CREATED));
    assert!(!client.implements(hook_id::TOTAL_HOOKS));

    // Implemented: called, and its returns arrive.
    let returns = within(client.on_deactivate(Z_OnDeactivateArgs {})).await;
    assert_eq!(render_typed(&returns), expected()["Z_OnDeactivateReturns"]);

    // Provided but not reported: never called.
    let skipped =
        within(client.user_has_been_created_with_rpc_err(Z_UserHasBeenCreatedArgs::default()))
            .await;
    assert!(skipped.1.is_none());
    assert_eq!(*plugin.calls.lock().unwrap(), ["OnDeactivate"]);

    // Reported but not provided: Go's server error, zero returns.
    let (returns, err) = within(client.on_install_with_rpc_err(Z_OnInstallArgs::default())).await;
    assert_eq!(returns, Default::default());
    match err {
        Some(go_netrpc::Error::Server(msg)) => {
            assert_eq!(msg, "Hook OnInstall called but not implemented.");
        }
        other => panic!("expected the not-implemented server error, got {other:?}"),
    }
    assert_eq!(
        within(client.on_install(Z_OnInstallArgs::default())).await,
        Default::default()
    );
}

#[tokio::test]
async fn rpc_an_unprovided_api_method_fails_as_in_go() {
    struct Nothing;
    impl PluginApi for Nothing {}

    let (a, b) = tokio::io::duplex(1 << 16);
    let mut server = Server::new();
    register_api(&mut server, &Arc::new(Nothing));
    tokio::spawn(Arc::new(server).serve(b));
    let client = go_netrpc::Client::new(a);
    let err = within(client.call::<_, mm_plugin::wire::plugin::Z_GetUserReturns>(
        "Plugin.GetUser",
        &mm_plugin::wire::plugin::Z_GetUserArgs { a: "u".into() },
    ))
    .await
    .unwrap_err();
    assert_eq!(err.to_string(), "API GetUser called but not implemented.");

    // The plugin's client logs the failure and answers zero values.
    let api = ApiClient::new(client);
    let returns =
        within(api.get_user(mm_plugin::wire::plugin::Z_GetUserArgs { a: "u".into() })).await;
    assert_eq!(returns, Default::default());
}
