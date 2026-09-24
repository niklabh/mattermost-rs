//! Port of `text/template/parse`: the lexer, the parse tree and the parser.

pub(crate) mod lex;
pub(crate) mod node;
#[allow(clippy::module_inception)]
pub(crate) mod parse;
