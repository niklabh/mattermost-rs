use std::sync::Arc;

use thiserror::Error;

/// Why a call, or a connection, failed. Cloneable, because a connection failure is delivered to
/// every call pending on it.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum Error {
    /// The server's method returned this error (Go's `rpc.ServerError`).
    #[error("{0}")]
    Server(String),

    /// The client was closed, or had already failed (Go's `rpc.ErrShutdown`).
    #[error("connection is shut down")]
    Shutdown,

    /// The connection ended while calls were pending and the client had not been closed (Go's
    /// `io.ErrUnexpectedEOF`).
    #[error("unexpected EOF")]
    UnexpectedEof,

    /// The reply could not be decoded into the requested type. This ends the connection, as in
    /// Go, whose gob stream cannot be resynchronised after a failed decode.
    #[error("reading body {0}")]
    ReadingBody(Arc<gobwire::Error>),

    /// A header or discarded body could not be decoded.
    #[error("{0}")]
    Gob(Arc<gobwire::Error>),

    #[error("{0}")]
    Io(Arc<std::io::Error>),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(Arc::new(e))
    }
}

impl From<gobwire::Error> for Error {
    fn from(e: gobwire::Error) -> Self {
        Error::Gob(Arc::new(e))
    }
}
