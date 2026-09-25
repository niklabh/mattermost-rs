//! lib/pq's errors (error.go) and the sentinels its driver returns.

/// `*pq.Error`: every field of the server's `ErrorResponse`, as `parseError` fills it.
///
/// `query` is lib/pq's unexported field: only [`PqError::go_error`] reads it, to place the
/// position; it never crosses a gob stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PqError {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub detail: String,
    pub hint: String,
    pub position: String,
    pub internal_position: String,
    pub internal_query: String,
    pub r#where: String,
    pub schema: String,
    pub table: String,
    pub column: String,
    pub data_type_name: String,
    pub constraint: String,
    pub file: String,
    pub line: String,
    pub routine: String,
    pub query: String,
}

impl PqError {
    /// `Error.Fatal`: severity `FATAL`, which lib/pq turns into `driver.ErrBadConn`.
    pub fn fatal(&self) -> bool {
        self.severity == "FATAL"
    }

    /// `(*Error).Error()`: `pq: <message>`, with the position placed in the query when both are
    /// known, and the SQLSTATE in parentheses.
    pub fn go_error(&self) -> String {
        let mut msg = self.message.clone();
        if !self.query.is_empty()
            && !self.position.is_empty()
            && let Ok(pos) = self.position.parse::<i64>()
        {
            let lines: Vec<&str> = self.query.split('\n').collect();
            let (line, col) = pos_to_line(pos, &lines);
            if lines.len() == 1 {
                msg.push_str(&format!(" at column {col}"));
            } else {
                msg.push_str(&format!(" at position {line}:{col}"));
            }
        }
        if self.code.is_empty() {
            format!("pq: {msg}")
        } else {
            format!("pq: {msg} ({})", self.code)
        }
    }
}

/// `posToLine`: the 1-based line and column of a 1-based character position.
fn pos_to_line(pos: i64, lines: &[&str]) -> (i64, i64) {
    let mut read = 0i64;
    let mut line = 0i64;
    let mut col = 0i64;
    for l in lines {
        line += 1;
        let ll = l.chars().count() as i64 + 1;
        if read + ll >= pos {
            col = (pos - read).max(1);
            break;
        }
        read += ll;
    }
    (line, col)
}

/// Every error this driver returns.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Error {
    /// `*pq.Error`, the server's `ErrorResponse`.
    #[error("{}", .0.go_error())]
    Pq(Box<PqError>),
    /// `driver.ErrBadConn`.
    #[error("driver: bad connection")]
    BadConn,
    /// `io.EOF`, which `Rows.Next` returns at the end of a result set.
    #[error("EOF")]
    Eof,
    /// A transport failure: Go's `*net.OpError` or an unexpected end of stream. `eof` is
    /// `io.EOF` / `io.ErrUnexpectedEOF`, which `handleError` turns into `ErrBadConn`.
    #[error("{message}")]
    Io { message: String, eof: bool },
    /// A write that sent nothing: lib/pq's `safeRetryError`, reported as `ErrBadConn`.
    #[error("{0}")]
    SafeRetry(String),
    /// Any other error lib/pq builds with `errors.New` or `fmt.Errorf`; the text is Go's.
    #[error("{0}")]
    Msg(String),
}

impl Error {
    pub(crate) fn msg(s: impl Into<String>) -> Self {
        Error::Msg(s.into())
    }

    pub(crate) fn io(e: &std::io::Error) -> Self {
        let eof = e.kind() == std::io::ErrorKind::UnexpectedEof;
        Error::Io {
            message: if eof {
                "unexpected EOF".to_owned()
            } else {
                e.to_string()
            },
            eof,
        }
    }
}

/// `errQueryInProgress`.
pub(crate) const QUERY_IN_PROGRESS: &str =
    "pq: there is already a query being processed on this connection";
/// `errUnexpectedReady`.
pub(crate) const UNEXPECTED_READY: &str = "unexpected ReadyForQuery";
/// `ErrInFailedTransaction`.
pub(crate) const IN_FAILED_TRANSACTION: &str =
    "pq: could not complete operation in a failed transaction";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_position_is_placed_on_a_line_and_a_column() {
        let mut e = PqError {
            message: "syntax error at or near \"FORM\"".into(),
            code: "42601".into(),
            position: "10".into(),
            query: "SELECT 1 FORM x".into(),
            ..PqError::default()
        };
        assert_eq!(
            e.go_error(),
            "pq: syntax error at or near \"FORM\" at column 10 (42601)"
        );
        e.query = "SELECT 1\nFORM x".into();
        assert_eq!(
            e.go_error(),
            "pq: syntax error at or near \"FORM\" at position 2:1 (42601)"
        );
        e.query.clear();
        e.code.clear();
        assert_eq!(e.go_error(), "pq: syntax error at or near \"FORM\"");
    }
}
