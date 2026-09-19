# goplugin

HashiCorp [go-plugin](https://github.com/hashicorp/go-plugin)'s net/rpc protocol for Rust, both
sides:

- **Host.** A Rust program launches go-plugin plugins written in Go, or reattaches to them,
  dispenses them and calls them.
- **Plugin.** A plugin written in Rust is launched and called by a Go host that uses go-plugin, and
  the host cannot tell it apart from a Go plugin.

It is built in layers, and each layer can be used on its own:

| Module | What it is |
|---|---|
| `yamux` | A multiplexer wire- and behaviour-compatible with `hashicorp/yamux`, the transport go-plugin runs over |
| `broker` | go-plugin's `MuxBroker`: extra streams on a connection, rendezvoused by id |
| `rpc` | The protocol on one connection: control, stdio and dispensing |
| `client` | The host: launching, handshake, checksum, reattach, kill |
| `server` | The plugin: `serve` from the plugin's own process |
| `hclog` | Parsing and writing the hclog lines plugins log to stderr |

Calls go through [`go-netrpc`](https://crates.io/crates/go-netrpc), and their arguments are
[`gobwire`](https://crates.io/crates/gobwire) values.

## A host

```rust
use goplugin::{Client, ClientConfig, HandshakeConfig, PluginCommand};

let mut config = ClientConfig::new(HandshakeConfig {
    protocol_version: 1,
    magic_cookie_key: "BASIC_PLUGIN".into(),
    magic_cookie_value: "hello".into(),
});
config.cmd = Some(PluginCommand::new("./plugin"));

let client = Client::start(config).await?;
let greeter = client.dispense("greeter").await?;
let greeting: String = greeter.client.call("Plugin.Greet", "Gopher").await?;
client.kill().await;
```

## A plugin

```rust
use go_netrpc::{Server, ServiceError};
use goplugin::{HandshakeConfig, MuxBroker, ServeConfig};

let config = ServeConfig::new(HandshakeConfig {
    protocol_version: 1,
    magic_cookie_key: "BASIC_PLUGIN".into(),
    magic_cookie_value: "hello".into(),
})
.plugin("greeter", |_broker: MuxBroker| {
    let mut server = Server::new();
    server.register("Plugin.Greet", |name: String| async move {
        Ok::<_, ServiceError>(format!("Hello, {name}!"))
    });
    Ok(server)
});
goplugin::serve(config).await?;
```

`examples/` has a complete pair. `kv_plugin` is a plugin that answers calls, dials back to a
server its host offers on the broker, offers a server of its own, and writes to the host's stdio
and log. `kv_host` drives it, or the Go plugin it mirrors:

```sh
cargo build --examples
cargo run --example kv_host -- target/debug/examples/kv_plugin
```

## Matching go-plugin

It is tested against go-plugin v1.8.0 and hashicorp/yamux v0.1.2. The transcript of a Go host
driving a Go plugin is the reference, and every other pairing must reproduce it exactly: a Rust
host with a Go plugin, a Go host with a Rust plugin, and Rust with Rust. The transcript covers
dispensing, calls and their errors, servers offered in both directions on the broker, stdio, hclog
output, ping, reattach, kill, and the checksum, cookie and version refusals.

## Not supported

- **The gRPC protocol.** A plugin that negotiates `grpc` is refused with
  `ClientError::UnsupportedProtocol`. Only `netrpc` is implemented.
- **Automatic mTLS.** A plugin that asks for it is refused with `ClientError::AutoMtls`.

Only unix platforms are tested. Elsewhere the plugin side listens on TCP, as go-plugin does, but
that path has no test.

## Licence

MPL-2.0, the licence of HashiCorp go-plugin, whose protocol this reimplements. This crate is not
affiliated with or endorsed by HashiCorp.
