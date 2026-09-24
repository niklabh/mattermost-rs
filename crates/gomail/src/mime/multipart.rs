//! Port of the writing half of Go's `mime/multipart` (writer.go) as go-mail drives it:
//! `NewWriter`, `SetBoundary`, `CreatePart` and `Close`, over an in-memory buffer.

/// Why `SetBoundary` refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BoundaryError {
    #[error("mime: SetBoundary called after write")]
    AfterWrite,
    #[error("mime: invalid boundary length")]
    InvalidLength,
    #[error("mime: invalid boundary character")]
    InvalidCharacter,
}

/// Port of `multipart.Writer`. Output accumulates in the caller's buffer, passed to each call —
/// go-mail writes a multipart body *through* its own writer, interleaved with other bytes.
#[derive(Debug, Clone)]
pub struct Writer {
    boundary: String,
    has_part: bool,
}

impl Writer {
    /// `NewWriter` (writer.go:28) with the boundary `randomBoundary` would have drawn — the
    /// caller supplies it so tests can fix it. Go's is 30 bytes from `crypto/rand` in lower-case
    /// hex; [`random_boundary`] is that.
    pub fn new(boundary: String) -> Self {
        Self {
            boundary,
            has_part: false,
        }
    }

    /// `Boundary` (writer.go:36).
    pub fn boundary(&self) -> &str {
        &self.boundary
    }

    /// `SetBoundary` (writer.go:48).
    pub fn set_boundary(&mut self, boundary: &str) -> Result<(), BoundaryError> {
        if self.has_part {
            return Err(BoundaryError::AfterWrite);
        }
        if boundary.is_empty() || boundary.len() > 70 {
            return Err(BoundaryError::InvalidLength);
        }
        // Go ranges over runes and compares the rune; a multi-byte rune is never in the set.
        let end = boundary.len() - 1;
        for (i, b) in boundary.char_indices() {
            if b.is_ascii_alphanumeric() {
                continue;
            }
            match b {
                '\'' | '(' | ')' | '+' | '_' | ',' | '-' | '.' | '/' | ':' | '=' | '?' => continue,
                ' ' if i != end => continue,
                _ => return Err(BoundaryError::InvalidCharacter),
            }
        }
        self.boundary = boundary.to_owned();
        Ok(())
    }

    /// `CreatePart` (writer.go:100): the delimiter, then each header in **sorted key order** (a
    /// key's values in their order), then a blank line. Keys are written as given — Go does not
    /// canonicalise here.
    pub fn create_part(&mut self, out: &mut Vec<u8>, header: &[(String, Vec<String>)]) {
        if self.has_part {
            out.extend_from_slice(b"\r\n--");
        } else {
            out.extend_from_slice(b"--");
        }
        out.extend_from_slice(self.boundary.as_bytes());
        out.extend_from_slice(b"\r\n");
        let mut keys: Vec<&(String, Vec<String>)> = header.iter().collect();
        keys.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        for (k, values) in keys {
            for v in values {
                out.extend_from_slice(k.as_bytes());
                out.extend_from_slice(b": ");
                out.extend_from_slice(v.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
        }
        out.extend_from_slice(b"\r\n");
        self.has_part = true;
    }

    /// `Close` (writer.go:175): the closing delimiter, preceded by a line break.
    pub fn close(&mut self, out: &mut Vec<u8>) {
        self.has_part = false;
        out.extend_from_slice(b"\r\n--");
        out.extend_from_slice(self.boundary.as_bytes());
        out.extend_from_slice(b"--\r\n");
    }
}

/// `randomBoundary` (writer.go:86): 30 bytes of cryptographic randomness in lower-case hex.
pub fn random_boundary() -> String {
    use rand::RngCore as _;
    let mut buf = [0u8; 30];
    rand::rng().fill_bytes(&mut buf);
    hex(&buf)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../../fixtures/behaviour_mail.json")).unwrap()
    }

    #[test]
    fn set_boundary_matches_go() {
        let oracle = oracle();
        for row in oracle["multipart"]["set_boundary"].as_array().unwrap() {
            let boundary = row["boundary"].as_str().unwrap();
            let mut w = Writer::new("x".into());
            if row["after_write"].as_bool() == Some(true) {
                w.create_part(&mut Vec::new(), &[]);
            }
            let got = w.set_boundary(boundary).err().map(|e| e.to_string());
            assert_eq!(got.as_deref(), row["error"].as_str(), "{boundary:?}");
            if got.is_none() {
                assert_eq!(w.boundary(), boundary);
            }
        }
    }

    #[test]
    fn writes_match_go() {
        let oracle = oracle();
        let rows = oracle["multipart"]["writes"].as_array().unwrap();
        assert_eq!(rows.len(), 4);
        for row in rows {
            let mut w = Writer::new("random".into());
            w.set_boundary(row["boundary"].as_str().unwrap()).unwrap();
            let mut out = Vec::new();
            for part in row["parts"].as_array().into_iter().flatten() {
                let header: Vec<(String, Vec<String>)> = part["header"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| {
                        let values = v
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|s| s.as_str().unwrap().to_owned())
                            .collect();
                        (k.clone(), values)
                    })
                    .collect();
                w.create_part(&mut out, &header);
                out.extend_from_slice(part["body"].as_str().unwrap().as_bytes());
            }
            w.close(&mut out);
            assert_eq!(
                String::from_utf8(out).unwrap(),
                row["output"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn random_boundary_is_sixty_hex_digits() {
        let b = random_boundary();
        assert_eq!(b.len(), 60);
        assert!(
            b.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(b, random_boundary());
    }
}
