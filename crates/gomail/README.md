# gomail

A byte-exact Rust port of the Go code a Go program sends e-mail through: the message writer of
[`github.com/wneessen/go-mail`](https://github.com/wneessen/go-mail) v0.8.1, and the parts of the
Go 1.26 standard library below it — `net/mail`, `net/smtp`, `net/textproto`, `mime`,
`mime/multipart`, `mime/quotedprintable` and `encoding/base64`.

```rust
use gomail::netmail::{Address, parse_address};

let a = parse_address(r#""Lovelace, Ada" <ada@example.com>"#).unwrap();
assert_eq!((a.name.as_str(), a.address.as_str()), ("Lovelace, Ada", "ada@example.com"));
assert_eq!(a.to_string(), r#""Lovelace, Ada" <ada@example.com>"#);

let zoe = Address { name: "Zoë".into(), address: "zoe@example.com".into() };
assert_eq!(zoe.to_string(), "=?utf-8?q?Zo=C3=AB?= <zoe@example.com>");

assert_eq!(parse_address("not an address").unwrap_err().to_string(), "mail: no angle-addr");
```

Every value above, the error text included, is what Go's `net/mail` returns.

Writing a message and sending it over SMTP:

```rust,no_run
use gomail::msg::{AddrHeader, Msg, SystemEnv};
use gomail::smtp::{Client, PlainAuth, TlsConfig, dial};
use std::time::Duration;

async fn send() -> Result<(), Box<dyn std::error::Error>> {
    let mut msg = Msg::new();
    msg.set_addr_header(AddrHeader::From, &["Ada <ada@example.com>"])?;
    msg.set_addr_header(AddrHeader::To, &["bob@example.com"])?;
    msg.set_gen_header("Subject", &["Hello"]);
    msg.set_body_string("text/html", "<p>Hello</p>");
    msg.add_alternative_string("text/plain", "Hello");
    let bytes = msg.write_to(&mut SystemEnv { type_by_extension: |_| String::new() })?;

    let conn = dial("smtp.example.com:587", Some(Duration::from_secs(30))).await?;
    let mut client = Client::new(conn, "smtp.example.com").await?;
    client.hello("localhost").await?;
    client.start_tls(&TlsConfig { server_name: "smtp.example.com".into(), insecure_skip_verify: false }).await?;
    client.auth(&mut PlainAuth {
        identity: String::new(),
        username: "ada".into(),
        password: "secret".into(),
        host: "smtp.example.com".into(),
    }).await?;
    client.mail("ada@example.com").await?;
    client.rcpt("bob@example.com").await?;
    let mut w = client.data().await?;
    w.write(&bytes).await?;
    w.close().await?;
    client.quit().await?;
    Ok(())
}
```

## Why a port

When a Rust program must put the same bytes on the wire as a Go program — the same headers,
folding, encoded words, boundaries and transfer encodings, and the same error text when an SMTP
server refuses — reusing a Rust mail stack gives *a* message, not *the* message. Each piece is
ported from its Go source and tested against Go: a Go program drives the real packages, and
against a scripted SMTP server, and the tests assert byte equality with what it records.

Every error's `Display` is Go's `err.Error()` for the same failure. The reference is Go
**1.26.4**, where `textproto` errors started quoting their message.

## Scope

What is ported is what a message writer and an SMTP client need: go-mail's `Msg` writer (not its
`Client`, which this crate replaces with the `net/smtp` client it wraps), and TLS through
`rustls` with certificate verification following `crypto/x509`'s order. The `testing` feature
exposes an SMTP sink used by the repository's tests; it is not part of the stable API.

## Licence

MIT (go-mail, `LICENSE-GO-MAIL`) and BSD-3-Clause (the Go standard library, `LICENSE-GO`).
