//! TEMPORARY STUB — replaced by the port of channels/utils/markdown.go on merge.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MarkdownError(pub String);
pub fn strip_markdown_and_decode(markdown: &str) -> Result<String, MarkdownError> {
    Ok(markdown.to_owned())
}
