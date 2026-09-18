use thiserror::Error;

/// Everything that can go wrong encoding or decoding a gob stream.
///
/// Messages follow Go's `encoding/gob` wording where one exists, so a log line from either side
/// of a Go↔Rust connection reads the same.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The input ended inside a value.
    #[error("gob: unexpected EOF")]
    UnexpectedEof,

    /// The value continues in a message that has not been pushed yet. Only
    /// [`Decoder::push_message`](crate::Decoder::push_message) surfaces this, as
    /// [`Progress::NeedMore`](crate::Progress::NeedMore).
    #[error("gob: value continues in a message not yet received")]
    NeedMore,

    /// An unsigned integer claimed more than eight bytes (decode.go, `errBadUint`).
    #[error("gob: encoded unsigned integer out of range")]
    BadUint,

    /// Structurally invalid data.
    #[error("gob: {0}")]
    Corrupt(String),

    /// A type id was used before it was defined.
    #[error("gob: bad data: undefined type {0}")]
    UndefinedType(i64),

    /// A type id was defined twice on one stream (decoder.go, `recvType`).
    #[error("gob: duplicate type received")]
    DuplicateType(i64),

    /// The remote type cannot be stored in the local one (decode.go, `compatibleType`).
    #[error("gob: {0}")]
    TypeMismatch(String),

    /// A number does not fit the local type (decode.go, `overflow`).
    #[error("gob: value for {0:?} out of range")]
    Overflow(&'static str),

    /// A string was not valid UTF-8. Go strings may hold arbitrary bytes; Rust `String`s may not.
    #[error("gob: string is not valid UTF-8")]
    InvalidUtf8,

    /// A value cannot be encoded, such as a nil pointer where Go would panic.
    #[error("gob: {0}")]
    Encode(String),

    /// A `GobEncoder`/`BinaryMarshaler`/`TextMarshaler` implementation failed.
    #[error("gob: marshaler: {0}")]
    Marshal(String),

    /// The dynamic representation cannot express this type.
    #[error("gob: {0}")]
    Unsupported(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
