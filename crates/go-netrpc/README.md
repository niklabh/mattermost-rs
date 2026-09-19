# go-netrpc

Go's [`net/rpc`](https://pkg.go.dev/net/rpc) with its default gob codec, for async Rust. A
`Client` calls a Go `rpc.Server`, and a `Server` answers a Go `rpc.Client`, over any tokio
`AsyncRead + AsyncWrite` stream: a TCP or unix socket, or one stream of a multiplexed session, as
HashiCorp's go-plugin uses it. Arguments and replies are [`gobwire`](https://crates.io/crates/gobwire)
values.

## Calling a Go server

```rust
use gobwire::Gob;

#[derive(Gob, Default)]
struct Args {
    #[gob(name = "A")]
    a: i64,
    #[gob(name = "B")]
    b: i64,
}

let stream = tokio::net::TcpStream::connect("127.0.0.1:1234").await?;
let client = go_netrpc::Client::new(stream);
let product: i64 = client.call("Arith.Multiply", &Args { a: 7, b: 8 }).await?;
```

A `Client` is cheap to clone, and calls on its clones may overlap: replies are matched to calls by
sequence number, as in Go.

## Serving a Go client

```rust
use std::sync::Arc;
use go_netrpc::{Server, ServiceError};

let mut server = Server::new();
server.register("Arith.Multiply", |args: Args| async move {
    Ok::<_, ServiceError>(args.a * args.b)
});
let server = Arc::new(server);

let listener = tokio::net::TcpListener::bind("127.0.0.1:1234").await?;
loop {
    let (stream, _) = listener.accept().await?;
    tokio::spawn(server.clone().serve(stream));
}
```

Go derives method names from a registered receiver's exported methods. Here they are given
explicitly as `"Service.Method"`. A `ServiceError`'s text becomes the reply's `Error`, which a Go
client returns as an `rpc.ServerError`.

## Matching Go

It is tested against Go's `net/rpc` in both directions: a Go client against this server, and this
client against a Go server. That includes calls that overlap, unknown services and methods,
arguments that fail to decode, and connections that close with calls pending. Each case answers
with the same bytes and error text Go produces.

## Licence

MIT or Apache-2.0, at your option.
