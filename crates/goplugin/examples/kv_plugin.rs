//! A go-plugin plugin written in Rust, runnable by a Go host (or a Rust one).
//!
//! It serves the same `kv` plugin as the Go program goplugin's interop tests run against
//! (`reference/dump/goplugin` in the [repository](https://github.com/niklabh/mattermost-rs)),
//! method for method. As an example of the API it shows the three things a real plugin does:
//! answer calls, reach back to a server the host offers on the broker, and offer a server of its
//! own.
//!
//! A plugin is started by its host, not by hand; run by hand it refuses. `kv_host` starts it:
//!
//! ```text
//! cargo build --examples
//! cargo run --example kv_host -- target/debug/examples/kv_plugin
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use go_netrpc::{Server, ServiceError};
use gobwire::Gob;
use goplugin::hclog::{Level, format_line};
use goplugin::rpc::Empty;
use goplugin::{HandshakeConfig, MuxBroker, PluginStdio, ServeConfig, ServeError};
use tokio::io::AsyncWriteExt;

#[derive(Gob, Debug, Default)]
struct PutArgs {
    #[gob(name = "Key")]
    key: String,
    #[gob(name = "Value")]
    value: String,
}

#[derive(Gob, Debug, Default)]
struct AddArgs {
    #[gob(name = "A")]
    a: i64,
    #[gob(name = "B")]
    b: i64,
}

#[derive(Gob, Debug, Default)]
struct AddViaArgs {
    #[gob(name = "BrokerID")]
    broker_id: u32,
    #[gob(name = "A")]
    a: i64,
    #[gob(name = "B")]
    b: i64,
}

type Stdio = Arc<tokio::sync::Mutex<Option<PluginStdio>>>;

fn kv(broker: MuxBroker, stdio: Stdio) -> Server {
    // One map per dispensed instance, as the Go plugin's `Server` makes a fresh `KV`.
    let map: Arc<Mutex<HashMap<String, String>>> = Arc::default();
    let mut s = Server::new();

    let m = map.clone();
    s.register("Plugin.Put", move |args: PutArgs| {
        let m = m.clone();
        async move {
            m.lock()
                .map_err(|_| ServiceError("poisoned".into()))?
                .insert(args.key, args.value);
            Ok::<_, ServiceError>(Empty {})
        }
    });
    let m = map.clone();
    s.register("Plugin.Get", move |key: String| {
        let m = m.clone();
        async move {
            let map = m.lock().map_err(|_| ServiceError("poisoned".into()))?;
            map.get(&key)
                .cloned()
                .ok_or_else(|| ServiceError(format!("not found: {key}")))
        }
    });

    let b = broker.clone();
    s.register("Plugin.AddVia", move |args: AddViaArgs| {
        let b = b.clone();
        async move {
            let stream = b.dial(args.broker_id).await?;
            let host = go_netrpc::Client::new(stream);
            let sum: i64 = host
                .call(
                    "Plugin.Add",
                    &AddArgs {
                        a: args.a,
                        b: args.b,
                    },
                )
                .await?;
            let _ = host.close().await;
            Ok::<_, ServiceError>(sum)
        }
    });

    let b = broker;
    s.register("Plugin.Offer", move |factor: i64| {
        let b = b.clone();
        async move {
            let id = b.next_id();
            let mut mul = Server::new();
            mul.register("Plugin.Mul", move |x: i64| async move {
                Ok::<_, ServiceError>(x * factor)
            });
            tokio::spawn({
                let b = b.clone();
                async move { b.accept_and_serve(id, Arc::new(mul)).await }
            });
            Ok::<_, ServiceError>(id)
        }
    });

    let io = stdio;
    s.register("Plugin.Print", move |text: String| {
        let io = io.clone();
        async move {
            let mut guard = io.lock().await;
            if let Some(stdio) = guard.as_mut() {
                stdio
                    .stdout
                    .write_all(format!("out:{text}\n").as_bytes())
                    .await?;
                stdio
                    .stderr
                    .write_all(format!("err:{text}\n").as_bytes())
                    .await?;
            }
            Ok::<_, ServiceError>(Empty {})
        }
    });

    s.register("Plugin.Log", |text: String| async move {
        eprintln!(
            "{}",
            format_line(Level::Info, "kv", &text, &[("key", "value".into())])
        );
        Ok::<_, ServiceError>(Empty {})
    });
    s
}

#[tokio::main]
async fn main() {
    let stdio: Stdio = Arc::default();
    let mut config = ServeConfig::new(HandshakeConfig {
        protocol_version: 1,
        magic_cookie_key: "GOPLUGIN_ORACLE".into(),
        magic_cookie_value: "hello".into(),
    });
    let for_plugin = stdio.clone();
    config = config.plugin("kv", move |broker: MuxBroker| {
        Ok(kv(broker, for_plugin.clone()))
    });
    let for_conn = stdio.clone();
    config.stdio = Arc::new(move |s| {
        if let Ok(mut slot) = for_conn.try_lock() {
            *slot = Some(s);
        }
    });
    match goplugin::serve(config).await {
        Ok(()) => {}
        Err(ServeError::NotAPlugin | ServeError::Misconfigured) => std::process::exit(1),
        Err(e) => {
            eprintln!("kv_plugin: {e}");
            std::process::exit(1);
        }
    }
}
