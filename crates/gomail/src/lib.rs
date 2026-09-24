//! Byte-exact ports of the Go code below Mattermost's `platform/shared/mail`: the standard
//! library's `net/mail`, `net/smtp`, `net/textproto`, `mime`, `mime/multipart`,
//! `mime/quotedprintable` and `encoding/base64` (BSD-3-Clause), and the message writer of
//! `github.com/wneessen/go-mail` v0.8.1 (MIT) — as far as Mattermost's `sendMail` drives them.
//!
//! The Go reference is **Go 1.26.4**, the toolchain `server/go.mod` pins, not whatever `go` is
//! on the path: 1.26.4 changed `textproto.Error` to quote its message (`550 "5.7.1 …"`),
//! `textproto`'s protocol errors likewise, and `net/mail`'s joining of encoded words.
//!
//! The Mattermost call sequence (`mail.go`, AGPL) lives in `mm_app::mail`; this crate never
//! depends on an `mm-*` crate.
//!
//! Every error's `Display` is Go's `err.Error()` for the same failure: these strings reach the
//! HTTP wire inside `app.admin.test_email.failure`.

pub mod base64;
pub mod mime;
pub mod msg;
pub mod netmail;
pub mod smtp;
pub mod strconv;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
