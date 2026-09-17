//! HashiCorp [go-plugin](https://github.com/hashicorp/go-plugin)'s net/rpc protocol for Rust.
//!
//! A Rust host can run go-plugin plugins written in Go, and a plugin written in Rust can be run
//! by a Go host. Built up in layers, each usable on its own:
//!
//! * [`yamux`]: a multiplexer compatible with `hashicorp/yamux`, the transport go-plugin runs over.
//! * [`broker`]: go-plugin's `MuxBroker`, rendezvousing extra streams by id.
//! * [`rpc`]: the protocol on one connection — control, stdio and dispensing.
//! * [`client`]: the host, launching or reattaching to plugin processes.
//! * [`server`]: the plugin, serving from its own process.
//! * [`hclog`]: the log lines plugins write to stderr.

pub mod broker;
pub mod client;
pub mod hclog;
pub mod rpc;
pub mod server;
pub mod yamux;

pub use broker::MuxBroker;
pub use client::{
    Client, ClientConfig, ClientError, HandshakeConfig, PluginAddr, PluginCommand, ReattachConfig,
};
pub use rpc::{Dispensed, PluginServer, PluginStdio, RpcClient};
pub use server::{ServeConfig, ServeError, serve};
