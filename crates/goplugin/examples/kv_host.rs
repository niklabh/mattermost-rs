//! A go-plugin host written in Rust: it launches the `kv` plugin and uses it.
//!
//! The plugin can be `kv_plugin`, the Rust plugin next to this example, or the Go plugin it
//! mirrors. The host cannot tell them apart.
//!
//! ```text
//! cargo build --examples
//! cargo run --example kv_host -- target/debug/examples/kv_plugin
//! cargo run --example kv_host -- /path/to/go-kv-plugin plugin    # extra arguments pass through
//! ```

use std::sync::Arc;

use go_netrpc::{Server, ServiceError};
use gobwire::Gob;
use goplugin::rpc::Empty;
use goplugin::{Client, ClientConfig, HandshakeConfig, PluginCommand};

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let Some(program) = args.next() else {
        eprintln!("usage: kv_host <plugin> [args...]");
        std::process::exit(2);
    };

    // The handshake both sides are built with. A mismatched cookie or protocol version is
    // refused before any RPC is made.
    let mut config = ClientConfig::new(HandshakeConfig {
        protocol_version: 1,
        magic_cookie_key: "GOPLUGIN_ORACLE".into(),
        magic_cookie_value: "hello".into(),
    });
    let mut cmd = PluginCommand::new(&program);
    for arg in args {
        cmd = cmd.arg(arg);
    }
    config.cmd = Some(cmd);
    // Refuse to run a binary other than the one we meant. A real host pins this checksum
    // rather than computing it from the file it is about to run.
    config.checksum = Some(goplugin::client::sha256_file(program.as_ref())?);

    let client = Client::start(config).await?;
    println!(
        "started: protocol={} version={}",
        client.protocol(),
        client.negotiated_version()
    );

    let kv = client.dispense("kv").await?;

    // Plain calls, and a server-side error coming back as `go_netrpc::Error::Server`.
    kv.client
        .call::<_, Empty>(
            "Plugin.Put",
            &PutArgs {
                key: "greeting".into(),
                value: "hello".into(),
            },
        )
        .await?;
    let value: String = kv.client.call("Plugin.Get", "greeting").await?;
    println!("get greeting = {value:?}");
    match kv.client.call::<_, String>("Plugin.Get", "missing").await {
        Ok(v) => println!("get missing = {v:?}"),
        Err(e) => println!("get missing: {e}"),
    }

    // Offer the plugin a server of ours on the broker, and have it call back into it.
    let id = kv.broker.next_id();
    let mut adder = Server::new();
    adder.register("Plugin.Add", |a: AddArgs| async move {
        Ok::<_, ServiceError>(a.a + a.b)
    });
    tokio::spawn({
        let broker = kv.broker.clone();
        async move { broker.accept_and_serve(id, Arc::new(adder)).await }
    });
    let sum: i64 = kv
        .client
        .call(
            "Plugin.AddVia",
            &AddViaArgs {
                broker_id: id,
                a: 2,
                b: 40,
            },
        )
        .await?;
    println!("2 + 40 via the host's server = {sum}");

    // The other direction: the plugin offers a server, and we dial it.
    let offered: u32 = kv.client.call("Plugin.Offer", &7i64).await?;
    let times7 = go_netrpc::Client::new(kv.broker.dial(offered).await?);
    let product: i64 = times7.call("Plugin.Mul", &6i64).await?;
    times7.close().await?;
    println!("6 * 7 via the plugin's server = {product}");

    client.ping().await?;
    client.kill().await;
    println!("killed: exited={}", client.exited());
    Ok(())
}
