# goqr

A byte-exact Rust port of [`rsc.io/qr`](https://pkg.go.dev/rsc.io/qr) as vendored at
`github.com/mattermost/rsc@v0.0.0-20160330161541-bbaefb05eaa0` (`qr`, `qr/coding`, `gf256`): text
in, a QR code out, and the PNG its own small encoder writes for it.

```rust
let code = goqr::encode("HELLO WORLD", goqr::Level::H).unwrap();
assert_eq!(code.size, 21);
let png = code.png();
assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
```

## Why a port

A QR code is not one picture: the encoding mode, the version, the mask and the PNG writer all
change the bytes. When a Rust program has to hand out the same image a Go program would — here,
Mattermost's MFA enrolment QR code — only the same algorithm gives the same bytes. Every PNG this
crate writes is checked against the Go library's own output for the same text.

## What it is, exactly

- Encoding picks the smallest of numeric, alphanumeric and byte mode for the **whole** text, then
  the smallest version that holds it at the requested level.
- The mask is always 0: the Go library never chose one (`TODO: Pick appropriate mask`).
- The PNG is 1-bit greyscale with a four-module white border at eight pixels per module, a
  `tEXt` comment, and a single fixed-Huffman deflate block — not what `image/png` would write.

## Licence

BSD-3-Clause, as the Go original; see `LICENSE`.
