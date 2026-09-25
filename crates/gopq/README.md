# gopq

A Rust port of the connection half of [`github.com/lib/pq`](https://github.com/lib/pq) v1.12.3 —
the `database/sql/driver` connection a Go program talks to Postgres through: the simple and
extended query protocols exactly as lib/pq drives them, its parameter encoding, its value
decoding (text and binary result formats), its error type, its transaction handling and its
sticky "bad connection" state.

It exists so that a Rust server can answer a Go plugin's raw driver calls (Mattermost's plugin
`Driver` RPC) with the values and errors lib/pq itself would have produced.

```rust,no_run
# async fn demo() -> Result<(), gopq::Error> {
let mut conn = gopq::Conn::connect("postgres://user:pass@localhost/db?sslmode=disable").await?;
let mut rows = conn.query("SELECT 1::int8, 'a'", &[]).await?;
let mut dest = vec![gopq::Value::Null; 2];
rows.next(&mut conn, &mut dest).await?;
assert_eq!(dest[0], gopq::Value::Int64(1));
# Ok(()) }
```

Not ported: TLS (`sslmode` other than `disable`), GSSAPI, `COPY`, `LISTEN`, multi-host DSNs,
`target_session_attrs`, and context cancellation.

MIT, as lib/pq; see `LICENSE`.
