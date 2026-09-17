//! HashiCorp [go-plugin](https://github.com/hashicorp/go-plugin)'s net/rpc protocol for Rust.
//!
//! Built up in layers, each usable on its own:
//!
//! * [`yamux`]: a multiplexer compatible with `hashicorp/yamux`, the transport go-plugin runs over.

pub mod yamux;
