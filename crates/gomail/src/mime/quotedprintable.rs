//! Port of Go's `mime/quotedprintable.Writer` (writer.go) in its default, non-binary mode — the
//! encoder go-mail runs every text part through.

/// `lineMaxLen` (writer.go:9).
const LINE_MAX_LEN: usize = 76;

const UPPERHEX: &[u8; 16] = b"0123456789ABCDEF";

/// Port of `quotedprintable.Writer` with `Binary` false, writing into an in-memory buffer.
///
/// State survives across [`Writer::write`] calls exactly as Go's does — the pending line and
/// whether the last byte was a `\r` — so a `\r\n` split across two writes is still one line
/// break.
#[derive(Debug, Default)]
pub struct Writer {
    out: Vec<u8>,
    line: Vec<u8>,
    cr: bool,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Writer.Write` (writer.go:40).
    pub fn write(&mut self, p: &[u8]) {
        let mut n = 0;
        for (i, &b) in p.iter().enumerate() {
            // Printable ASCII other than '=', whitespace, and (non-binary) CR/LF pass through.
            if (b'!'..=b'~').contains(&b) && b != b'=' {
                continue;
            }
            if is_whitespace(b) || b == b'\n' || b == b'\r' {
                continue;
            }
            if i > n {
                self.write_raw(&p[n..i]);
                n = i;
            }
            self.encode(b);
            n += 1;
        }
        if n < p.len() {
            self.write_raw(&p[n..]);
        }
    }

    /// `Writer.Close` (writer.go:73): encode a trailing space or tab and flush.
    pub fn close(mut self) -> Vec<u8> {
        self.check_last_byte();
        self.flush();
        self.out
    }

    /// `write` (writer.go:83).
    fn write_raw(&mut self, p: &[u8]) {
        for &b in p {
            if b == b'\n' || b == b'\r' {
                if self.cr && b == b'\n' {
                    self.cr = false;
                    continue;
                }
                if b == b'\r' {
                    self.cr = true;
                }
                self.check_last_byte();
                self.insert_crlf();
                continue;
            }
            if self.line.len() == LINE_MAX_LEN - 1 {
                self.insert_soft_line_break();
            }
            self.line.push(b);
            self.cr = false;
        }
    }

    /// `encode` (writer.go:117).
    fn encode(&mut self, b: u8) {
        if LINE_MAX_LEN - 1 - self.line.len() < 3 {
            self.insert_soft_line_break();
        }
        self.line.push(b'=');
        self.line.push(UPPERHEX[usize::from(b >> 4)]);
        self.line.push(UPPERHEX[usize::from(b & 0x0f)]);
    }

    /// `checkLastByte` (writer.go:135): a line may not end in whitespace, so encode it.
    fn check_last_byte(&mut self) {
        if let Some(&b) = self.line.last() {
            if is_whitespace(b) {
                self.line.pop();
                self.encode(b);
            }
        }
    }

    fn insert_soft_line_break(&mut self) {
        self.line.push(b'=');
        self.insert_crlf();
    }

    fn insert_crlf(&mut self) {
        self.line.extend_from_slice(b"\r\n");
        self.flush();
    }

    fn flush(&mut self) {
        self.out.append(&mut self.line);
    }
}

fn is_whitespace(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_matches_go() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../../fixtures/behaviour_mail.json")).unwrap();
        let rows = oracle["quoted_printable"].as_array().unwrap();
        let mut checked = 0;
        for row in rows {
            let writes: Vec<&str> = row["writes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| w.as_str().unwrap())
                .collect();
            if writes.iter().any(|w| w.contains('\u{fffd}')) {
                continue;
            }
            let mut w = Writer::new();
            for chunk in &writes {
                w.write(chunk.as_bytes());
            }
            assert_eq!(
                String::from_utf8(w.close()).unwrap(),
                row["output"].as_str().unwrap(),
                "{writes:?}"
            );
            checked += 1;
        }
        assert!(checked > 25, "{checked}");
    }

    /// Bytes JSON cannot carry: 0xFF and friends are encoded, not passed through.
    #[test]
    fn high_bytes_are_encoded() {
        let mut w = Writer::new();
        w.write(b"\x00\x01\x7f\xff");
        assert_eq!(w.close(), b"=00=01=7F=FF");
    }
}
