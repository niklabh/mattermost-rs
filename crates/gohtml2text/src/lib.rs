//! Port of `github.com/jaytaylor/html2text` (SKELETON — API contract only).

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Parse(String),
}

/// `html2text.FromString(input)` with default `Options`.
pub fn from_string(_input: &str) -> Result<String, Error> {
    Err(Error::Parse("not implemented".into()))
}
