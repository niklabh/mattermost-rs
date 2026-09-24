//! Port of `html/template/error.go`: the escaper's error type.

use std::fmt;
use std::sync::Arc;

use crate::parse::node::{Src, error_location};

/// `ErrorCode` (error.go:30). Only the codes this port raises are listed; Go's own error string
/// does not include the code, so it is kept for the reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorCode {
    AmbigContext,
    BadHtml,
    BranchEnd,
    EndContext,
    NoSuchTemplate,
    OutputContext,
    PartialCharset,
    PartialEscape,
    SlashAmbig,
    PredefinedEscaper,
}

/// The node an error points at: its position and the text it was parsed from.
#[derive(Debug, Clone)]
pub(crate) struct ErrNode {
    pub pos: usize,
    pub src: Option<Arc<Src>>,
}

/// `Error` (error.go:13).
#[derive(Debug, Clone)]
pub(crate) struct EscError {
    #[allow(dead_code)]
    pub code: ErrorCode,
    pub node: Option<ErrNode>,
    pub name: String,
    pub line: usize,
    pub description: String,
}

/// `errorf` (error.go:245).
pub(crate) fn errorf(
    code: ErrorCode,
    node: Option<ErrNode>,
    line: usize,
    description: String,
) -> EscError {
    EscError {
        code,
        node,
        name: String::new(),
        line,
        description,
    }
}

impl fmt::Display for EscError {
    /// `Error.Error` (error.go:232).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(n) = &self.node {
            let empty = Src {
                parse_name: String::new(),
                text: Arc::from(""),
            };
            let loc = error_location(n.pos, n.src.as_deref().unwrap_or(&empty));
            return write!(f, "html/template:{loc}: {}", self.description);
        }
        if self.line != 0 {
            return write!(
                f,
                "html/template:{}:{}: {}",
                self.name, self.line, self.description
            );
        }
        if !self.name.is_empty() {
            return write!(f, "html/template:{}: {}", self.name, self.description);
        }
        write!(f, "html/template: {}", self.description)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_forms() {
        let mut e = errorf(ErrorCode::BadHtml, None, 0, "bad".into());
        assert_eq!(e.to_string(), "html/template: bad");
        e.name = "t".into();
        assert_eq!(e.to_string(), "html/template:t: bad");
        e.line = 3;
        assert_eq!(e.to_string(), "html/template:t:3: bad");
        e.node = Some(ErrNode {
            pos: 2,
            src: Some(Arc::new(Src {
                parse_name: "f".into(),
                text: Arc::from("a\nbc"),
            })),
        });
        assert_eq!(e.to_string(), "html/template:f:2:0: bad");
    }
}
