//! Port of Go's `net/smtp` client (smtp.go, auth.go) on tokio, with the transport underneath it
//! (`net/textproto`, the dialer, TLS). Every error's `Display` is Go's `err.Error()`.

mod client;
mod conn;
pub mod net;
pub mod textproto;
pub mod tls;

use std::sync::Arc;

pub use client::{Auth, Client, DataWriter, PlainAuth, ServerInfo};
pub use conn::{Conn, DialTlsError, dial, dial_tls};
pub use net::DialError;
pub use tls::{TlsConfig, TlsError, X509Error};

use crate::base64::CorruptInputError;

/// A failure of an SMTP exchange, with Go's text.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// A reply with an unexpected code (`textproto.Error`) or a malformed reply
    /// (`textproto.ProtocolError`).
    #[error(transparent)]
    Response(#[from] textproto::ResponseError),
    /// `io.EOF`: the server hung up.
    #[error("EOF")]
    Eof,
    /// A transport error in `net.OpError` form.
    #[error("{0}")]
    Io(String),
    /// A TLS failure.
    #[error(transparent)]
    Tls(#[from] TlsError),
    /// `validateLine` (smtp.go:437).
    #[error("smtp: A line must not contain CR or LF")]
    LineContainsCrLf,
    /// `Client.Hello` after another command.
    #[error("smtp: Hello called after other methods")]
    HelloAfterOtherMethods,
    /// An [`Auth`] mechanism refused.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// A 334 challenge that is not base64.
    #[error(transparent)]
    Base64(#[from] CorruptInputError),
    /// A read or write after `Close`.
    #[error("use of closed network connection")]
    Closed,
}

impl From<textproto::Error> for Error {
    fn from(e: textproto::Error) -> Self {
        Error::Response(e.into())
    }
}

/// The errors an [`Auth`] mechanism returns: `PlainAuth`'s three, and any other mechanism's
/// own (Mattermost's `loginAuth` lives in mm-app).
#[derive(Debug, Clone, thiserror::Error)]
pub enum AuthError {
    #[error("unencrypted connection")]
    UnencryptedConnection,
    #[error("wrong host name")]
    WrongHostName,
    #[error("unexpected server challenge")]
    UnexpectedServerChallenge,
    #[error(transparent)]
    Other(Arc<dyn std::error::Error + Send + Sync>),
}
