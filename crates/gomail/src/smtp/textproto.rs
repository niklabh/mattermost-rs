//! Port of the parts of Go's `net/textproto` (Go 1.26.4) that `net/smtp` uses: the reply reader
//! (`ReadResponse`, reader.go:287), its errors, and the DATA dot-writer (writer.go:68).
//!
//! Go 1.26.4 quotes: `textproto.Error` is `"%03d %q"` and the protocol errors quote the offending
//! line, so a rejected MAIL reads `550 "5.7.1 sender rejected"`, not `550 5.7.1 sender rejected`.

use crate::strconv;

/// Port of `textproto.Error` (textproto.go:37): a server reply with an unexpected code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code:03} {}", strconv::quote(.msg))]
pub struct Error {
    pub code: i32,
    pub msg: String,
}

/// Port of `textproto.ProtocolError` (textproto.go:48) — the three texts `ReadResponse` makes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("short response: {}", strconv::quote(.0))]
    ShortResponse(String),
    #[error("invalid response code: {}", strconv::quote(.0))]
    InvalidResponseCode(String),
}

/// Why a reply was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResponseError {
    #[error(transparent)]
    Reply(#[from] Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// `parseCodeLine` (reader.go:216). On a code mismatch Go still returns the code and message
/// alongside the error, and `ReadResponse` uses them.
struct CodeLine {
    code: i32,
    continued: bool,
    message: String,
    err: Option<ResponseError>,
}

fn parse_code_line(line: &str, expect_code: i32) -> CodeLine {
    let bytes = line.as_bytes();
    if bytes.len() < 4 || (bytes[3] != b' ' && bytes[3] != b'-') {
        return CodeLine {
            code: 0,
            continued: false,
            message: String::new(),
            err: Some(ProtocolError::ShortResponse(line.to_owned()).into()),
        };
    }
    let continued = bytes[3] == b'-';
    // `strconv.Atoi(line[0:3])`: Go keeps whatever Atoi returned (0 on failure, else the value,
    // which may be below 100 or negative) and reports the line as invalid.
    let parsed = go_atoi3(&line[..3]);
    let code = parsed.unwrap_or(0);
    if parsed.is_none() || code < 100 {
        return CodeLine {
            code,
            continued,
            message: String::new(),
            err: Some(ProtocolError::InvalidResponseCode(line.to_owned()).into()),
        };
    }
    let message = line[4..].to_owned();
    let mismatch = (1..10).contains(&expect_code) && code / 100 != expect_code
        || (10..100).contains(&expect_code) && code / 10 != expect_code
        || (100..1000).contains(&expect_code) && code != expect_code;
    let err = mismatch.then(|| {
        Error {
            code,
            msg: message.clone(),
        }
        .into()
    });
    CodeLine {
        code,
        continued,
        message,
        err,
    }
}

/// `strconv.Atoi` over the three code bytes. A leading `+` or `-` is accepted by Go; a negative
/// result then fails the `code < 100` check just as it does in Go.
fn go_atoi3(s: &str) -> Option<i32> {
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'+') => (false, &s[1..]),
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i32 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// The outcome of [`read_response`]: Go's `(code, message, err)` triple, all three at once —
/// `net/smtp`'s `Auth` loop reads the code and message of a reply it treats as an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub code: i32,
    pub message: String,
    pub err: Option<ResponseError>,
}

/// A source of lines as `textproto.Reader.ReadLine` returns them.
pub trait LineSource {
    type Error;
    /// One line without its `\n` or `\r\n`; `Err` at end of input.
    fn read_line(&mut self) -> impl std::future::Future<Output = Result<String, Self::Error>>;
}

/// Port of `Reader.ReadResponse` (reader.go:287) over any [`LineSource`].
pub async fn read_response<L: LineSource>(
    lines: &mut L,
    expect_code: i32,
) -> Result<Response, L::Error> {
    let first = lines.read_line().await?;
    let parsed = parse_code_line(&first, expect_code);
    let code = parsed.code;
    let multi = parsed.continued;
    let mut continued = parsed.continued;
    let mut err = parsed.err;
    let mut message = parsed.message;
    while continued {
        let line = lines.read_line().await?;
        let next = parse_code_line(&line, 0);
        if next.err.is_some() || next.code != code {
            message.push('\n');
            message.push_str(line.trim_end_matches(['\r', '\n']));
            continued = true;
            continue;
        }
        continued = next.continued;
        message.push('\n');
        message.push_str(&next.message);
    }
    if err.is_some() && multi && !message.is_empty() {
        err = Some(
            Error {
                code,
                msg: message.clone(),
            }
            .into(),
        );
    }
    Ok(Response { code, message, err })
}

/// `bufio.Reader.ReadLine` semantics over a byte buffer that the caller refills: the line up to
/// `\n`, with the `\n` and a single preceding `\r` removed. A final line without `\n` is
/// returned as-is (a trailing `\r` kept), and the next call reports end of input.
pub fn take_line(buf: &mut Vec<u8>, at_eof: bool) -> Option<Vec<u8>> {
    if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let mut line: Vec<u8> = buf.drain(..=pos).collect();
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        return Some(line);
    }
    if at_eof && !buf.is_empty() {
        return Some(std::mem::take(buf));
    }
    None
}

/// Port of `textproto.dotWriter` (writer.go:68): dot-stuffing, bare `\n` promoted to `\r\n`,
/// and the terminating `.\r\n` on close.
#[derive(Debug, Default)]
pub struct DotWriter {
    state: DotState,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum DotState {
    /// `wstateBegin`: nothing written yet.
    #[default]
    Begin,
    /// `wstateBeginLine`.
    BeginLine,
    /// `wstateCR`: wrote `\r`, possibly at end of line.
    Cr,
    /// `wstateData`.
    Data,
}

impl DotWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// `dotWriter.Write` (writer.go:84).
    pub fn write(&mut self, b: &[u8], out: &mut Vec<u8>) {
        for &c in b {
            match self.state {
                DotState::Begin | DotState::BeginLine | DotState::Data => {
                    if self.state != DotState::Data {
                        self.state = DotState::Data;
                        if c == b'.' {
                            // escape leading dot
                            out.push(b'.');
                        }
                    }
                    if c == b'\r' {
                        self.state = DotState::Cr;
                    }
                    if c == b'\n' {
                        out.push(b'\r');
                        self.state = DotState::BeginLine;
                    }
                }
                DotState::Cr => {
                    self.state = DotState::Data;
                    if c == b'\n' {
                        self.state = DotState::BeginLine;
                    }
                }
            }
            out.push(c);
        }
    }

    /// `dotWriter.Close` (writer.go:117): finish the line if needed, then `.\r\n`.
    pub fn close(self, out: &mut Vec<u8>) {
        match self.state {
            DotState::BeginLine => {}
            DotState::Cr => out.push(b'\n'),
            DotState::Begin | DotState::Data => out.extend_from_slice(b"\r\n"),
        }
        out.extend_from_slice(b".\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../../fixtures/behaviour_mail.json")).unwrap()
    }

    struct Lines {
        buf: Vec<u8>,
    }

    impl LineSource for Lines {
        type Error = &'static str;
        async fn read_line(&mut self) -> Result<String, Self::Error> {
            take_line(&mut self.buf, true)
                .map(|l| String::from_utf8(l).unwrap())
                .ok_or("EOF")
        }
    }

    #[tokio::test]
    async fn read_response_matches_go() {
        let oracle = oracle();
        let rows = oracle["read_response"].as_array().unwrap();
        assert!(rows.len() > 25);
        for row in rows {
            let input = row["input"].as_str().unwrap();
            let expect = i32::try_from(row["expect"].as_i64().unwrap()).unwrap();
            let mut lines = Lines {
                buf: input.as_bytes().to_vec(),
            };
            match read_response(&mut lines, expect).await {
                Ok(r) => {
                    assert_eq!(
                        i64::from(r.code),
                        row["code"].as_i64().unwrap(),
                        "{input:?}"
                    );
                    assert_eq!(r.message, row["message"].as_str().unwrap(), "{input:?}");
                    assert_eq!(
                        r.err.map(|e| e.to_string()).as_deref(),
                        row["error"].as_str(),
                        "{input:?} expect {expect}"
                    );
                }
                Err(e) => {
                    // Go returns (0, "", EOF) when the reader runs dry.
                    assert_eq!(row["error"].as_str(), Some(e), "{input:?}");
                    assert_eq!(row["code"].as_u64(), Some(0));
                }
            }
        }
    }

    #[test]
    fn dot_writer_matches_go() {
        let oracle = oracle();
        let rows = oracle["dot_writer"].as_array().unwrap();
        assert!(rows.len() > 10);
        for row in rows {
            let mut w = DotWriter::new();
            let mut out = Vec::new();
            let writes: Vec<&str> = row["writes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| w.as_str().unwrap())
                .collect();
            for chunk in &writes {
                w.write(chunk.as_bytes(), &mut out);
            }
            w.close(&mut out);
            assert_eq!(
                String::from_utf8(out).unwrap(),
                row["output"].as_str().unwrap(),
                "{writes:?}"
            );
        }
    }
}
