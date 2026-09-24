//! Byte-exact port of Go's `text/template` and `html/template` (Go 1.26).
//!
//! Mattermost renders every e-mail through `html/template` (`platform/shared/templates`), and the
//! bytes it produces are what recipients' mail clients see — including the escaper's choices in
//! every context (text, attributes, URLs, CSS, JavaScript) and its silent stripping of HTML
//! comments. A different template engine produces *an* e-mail, not *the* e-mail, so this crate
//! ports the Go packages line by line:
//!
//! * [`parse`](crate::parse): `text/template/parse` — lexer, parse tree, parser.
//! * `exec`, `funcs`, `fmt`, `strconv`: `text/template`'s executor and builtins, with the parts
//!   of `fmt` and `strconv` they print through.
//! * `html`: `html/template`'s contextual autoescaper — contexts, transitions, the escaper that
//!   rewrites pipelines, and every escaping function.
//!
//! The oracle is `reference/dump/behaviour_gotemplate.go` (fixture
//! `fixtures/behaviour_gotemplate.json`), which runs the real Go packages — including every
//! template Mattermost ships, through Mattermost's own `templates.New` — and the `go_parity` test
//! module asserts byte equality against it.
//!
//! This crate contains no Mattermost code and never depends on an `mm-*` crate.

mod exec;
mod fmt;
mod funcs;
mod html;
pub(crate) mod parse;
mod rv;
mod strconv;
mod text;
mod unicode_tables;
mod value;

#[cfg(test)]
mod go_parity;

pub use exec::MissingKey;
pub use html::template::HtmlTemplates;
pub use text::TextTemplates;
pub use value::Value;

/// A template error. Its `Display` is exactly Go's `err.Error()`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A parse error (`template: <name>:<line>: ...`).
    #[error("{0}")]
    Parse(String),
    /// An execution error (`template: <name>:<line>:<col>: executing ...`), or a lookup failure.
    #[error("{0}")]
    Exec(String),
    /// An `html/template` escaping error (`html/template:...`).
    #[error("{0}")]
    Escape(String),
}
