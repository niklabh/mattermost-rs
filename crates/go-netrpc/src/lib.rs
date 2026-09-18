//! Go's [`net/rpc`](https://pkg.go.dev/net/rpc) with its default gob codec, for async Rust.
//!
//! A [`Client`] calls methods on a Go `rpc.Server` (or any server speaking the protocol), and a
//! [`Server`] answers calls from a Go `rpc.Client`. Both run over any `AsyncRead + AsyncWrite`
//! byte stream — a TCP or unix socket, or one multiplexed stream of a yamux session as
//! HashiCorp's go-plugin uses.
//!
//! ```no_run
//! # async fn demo() -> Result<(), go_netrpc::Error> {
//! use gobwire::Gob;
//!
//! #[derive(Gob, Default)]
//! struct Args {
//!     #[gob(name = "A")]
//!     a: i64,
//!     #[gob(name = "B")]
//!     b: i64,
//! }
//!
//! let stream = tokio::net::TcpStream::connect("127.0.0.1:1234").await?;
//! let client = go_netrpc::Client::new(stream);
//! let product: i64 = client.call("Arith.Multiply", &Args { a: 7, b: 8 }).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # The protocol (net/rpc/client.go, server.go)
//!
//! Each call is two gob values on the connection's single gob stream: a `Request{ServiceMethod,
//! Seq}` header, then the argument. Each reply is a `Response{ServiceMethod, Seq, Error}` header,
//! then the reply — or, when `Error` is set, an empty struct in its place. Calls are matched to
//! replies by `Seq`, so they may overlap; the server runs each call concurrently.

mod client;
mod error;
mod frame;
mod messages;
mod server;

pub use client::Client;
pub use error::Error;
pub use messages::{InvalidRequest, Request, Response};
pub use server::{Server, ServiceError};
