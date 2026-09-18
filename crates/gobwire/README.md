# gobwire

Go's [`encoding/gob`](https://pkg.go.dev/encoding/gob) wire format for Rust: the byte streams
`gob.Encoder` and `gob.Decoder` exchange, and so the payloads of Go's `net/rpc` and HashiCorp
`go-plugin`'s net/rpc protocol.

```rust
use gobwire::{Encoder, Gob, StreamDecoder};

#[derive(Gob, Debug, Default, PartialEq)]
struct Point {
    #[gob(name = "X")]
    x: i64,
    #[gob(name = "Y")]
    y: i64,
}

let bytes = Encoder::new().encode(&Point { x: 22, y: 33 }).unwrap();
let mut decoder = StreamDecoder::new(&bytes[..]);
assert_eq!(decoder.decode::<Point>().unwrap(), Some(Point { x: 22, y: 33 }));
```

The bytes above are exactly the worked example in Go's `encoding/gob` documentation.

## Matching Go

It is tested against Go's own `encoding/gob` in both directions: every stream in a corpus written
by Go must decode in Rust to what Go's decoder produced, and every value Rust re-encodes must
decode in Go to the same thing.

- **Fields match by Go field name.** Name them with `#[gob(name = "...")]`.
- **Zero values are omitted exactly where Go omits them** — and structs and arrays are not.
- **Decoding merges into the destination**, as Go does: absent fields keep their values,
  pointers and slices are reused, map entries are replaced.
- **A value can span several messages** (whenever an interface introduces a new type); the
  decoder handles it without leaving a destination half-written.
- **Interfaces** (`any`, `error`) decode to a dynamic, re-encodable `Interface`, with no global
  type registry.
- **`time.Time`** is `GoTime`, byte-compatible with Go's binary form.

## Where it differs, and why

- A Rust map cannot be nil, so an empty map is sent as nil.
- A slice decoded into a `Vec` merges into existing elements only when the `Vec` is at least as
  long as the incoming slice; Go looks at capacity.
- Strings must be UTF-8.
- Nesting is limited (`MAX_DEPTH`) rather than bounded only by the stack.
- Go's decoder cannot skip an interface that defines a type inline, nor a nil interface inside
  a skipped field. `gobwire` skips both correctly, and its encoder always defines types before
  the value, so Go can skip anything `gobwire` sends — except a nil interface, which no encoding
  can protect Go's skip from.
- gob ignores `encoding.TextMarshaler` whatever its documentation says; so does `gobwire`.

## Licence

MIT or Apache-2.0, at your option.
