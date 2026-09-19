# gobwire-derive

`#[derive(Gob)]` for [`gobwire`](https://crates.io/crates/gobwire), Go's `encoding/gob` wire
format for Rust. Use it through `gobwire`, which re-exports it under its default `derive`
feature; there is no reason to depend on this crate directly.

```rust
use gobwire::Gob;

#[derive(Gob, Debug, Default)]
#[gob(name = "Post")]
struct Post {
    #[gob(name = "Id")]
    id: String,
    #[gob(name = "IPAddress")]
    ip_address: String,
    #[gob(skip)]
    cached: bool,
}
```

The derive implements `GobType`, `Encode` and `Decode` for a struct with named fields.

| Attribute | On | Meaning |
|---|---|---|
| `#[gob(name = "...")]` | field | The Go field name, which the decoder matches on. Defaults to the field name in PascalCase (`request_id` → `RequestId`). That is wrong for Go initialisms such as `IPAddress`, so name those explicitly. |
| `#[gob(name = "...")]` | struct | The type name sent in its definition. It is informational: Go's decoder does not match on it. |
| `#[gob(skip)]` | field | Neither sent nor received, like an unexported Go field. |
| `#[gob(transparent)]` | struct with one field | Encode as that field, for Go named types such as `type StringMap map[string]string`. |

## Licence

MIT or Apache-2.0, at your option.
