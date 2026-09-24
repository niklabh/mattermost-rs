//! Port of RFC 2047 encoded words from Go's `mime/encodedword.go` (Go 1.26.4): `WordEncoder`,
//! which go-mail runs over every generic header value and `net/mail` over a display name, and
//! the `WordDecoder.Decode` that `net/mail` uses on each atom of a phrase.

use crate::base64;

/// Port of `mime.WordEncoder` (encodedword.go:20): `BEncoding` or `QEncoding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordEncoder {
    /// `mime.BEncoding` — base64.
    B,
    /// `mime.QEncoding` — the Q encoding of RFC 2047 §4.2.
    Q,
}

/// `maxEncodedWordLen` (encodedword.go:88).
const MAX_ENCODED_WORD_LEN: usize = 75;
/// `maxContentLen` (encodedword.go:90): 75 - len("=?UTF-8?q?") - len("?=") — computed from the
/// literal `UTF-8`, whatever charset is actually in use.
const MAX_CONTENT_LEN: usize = MAX_ENCODED_WORD_LEN - "=?UTF-8?q?".len() - "?=".len();
/// `maxBase64Len` (encodedword.go:94): `StdEncoding.DecodedLen(maxContentLen)`.
const MAX_BASE64_LEN: usize = MAX_CONTENT_LEN / 4 * 3;

const UPPERHEX: &[u8; 16] = b"0123456789ABCDEF";

impl WordEncoder {
    fn letter(self) -> char {
        match self {
            WordEncoder::B => 'b',
            WordEncoder::Q => 'q',
        }
    }

    /// Port of `WordEncoder.Encode` (encodedword.go:33): `s` unchanged unless it holds a byte
    /// outside printable ASCII other than tab, else one or more encoded words separated by a
    /// space, each at most 75 bytes when the charset is UTF-8 (split at rune boundaries).
    pub fn encode(self, charset: &str, s: &str) -> String {
        if !needs_encoding(s) {
            return s.to_owned();
        }
        let mut buf = String::with_capacity(48);
        self.open_word(&mut buf, charset);
        match self {
            WordEncoder::B => self.b_encode(&mut buf, charset, s),
            WordEncoder::Q => self.q_encode(&mut buf, charset, s),
        }
        buf.push_str("?=");
        buf
    }

    fn open_word(self, buf: &mut String, charset: &str) {
        buf.push_str("=?");
        buf.push_str(charset);
        buf.push('?');
        buf.push(self.letter());
        buf.push('?');
    }

    fn split_word(self, buf: &mut String, charset: &str) {
        buf.push_str("?=");
        buf.push(' ');
        self.open_word(buf, charset);
    }

    /// `bEncode` (encodedword.go:97). Go streams through a `base64.NewEncoder` that is closed at
    /// each split, so every chunk is padded independently — the same as encoding each chunk.
    fn b_encode(self, buf: &mut String, charset: &str, s: &str) {
        let encoded_len = s.len().div_ceil(3) * 4;
        if !is_utf8(charset) || encoded_len <= MAX_CONTENT_LEN {
            buf.push_str(&base64::encode(s.as_bytes()));
            return;
        }
        let mut current_len = 0;
        let mut last = 0;
        for (i, c) in s.char_indices() {
            let rune_len = c.len_utf8();
            if current_len + rune_len <= MAX_BASE64_LEN {
                current_len += rune_len;
            } else {
                buf.push_str(&base64::encode(&s.as_bytes()[last..i]));
                self.split_word(buf, charset);
                last = i;
                current_len = rune_len;
            }
        }
        buf.push_str(&base64::encode(&s.as_bytes()[last..]));
    }

    /// `qEncode` (encodedword.go:127).
    fn q_encode(self, buf: &mut String, charset: &str, s: &str) {
        if !is_utf8(charset) {
            write_q_string(buf, s.as_bytes());
            return;
        }
        let mut current_len = 0;
        for (i, c) in s.char_indices() {
            let b = s.as_bytes()[i];
            let rune_len = c.len_utf8();
            let enc_len = if (b' '..=b'~').contains(&b) && b != b'=' && b != b'?' && b != b'_' {
                1
            } else {
                3 * rune_len
            };
            if current_len + enc_len > MAX_CONTENT_LEN {
                self.split_word(buf, charset);
                current_len = 0;
            }
            write_q_string(buf, &s.as_bytes()[i..i + rune_len]);
            current_len += enc_len;
        }
    }
}

/// `needsEncoding` (encodedword.go:41).
fn needs_encoding(s: &str) -> bool {
    s.chars().any(|c| !(' '..='~').contains(&c) && c != '\t')
}

/// `writeQString` (encodedword.go:158).
fn write_q_string(buf: &mut String, s: &[u8]) {
    for &b in s {
        match b {
            b' ' => buf.push('_'),
            b'!'..=b'~' if b != b'=' && b != b'?' && b != b'_' => buf.push(char::from(b)),
            _ => {
                buf.push('=');
                buf.push(char::from(UPPERHEX[usize::from(b >> 4)]));
                buf.push(char::from(UPPERHEX[usize::from(b & 0x0f)]));
            }
        }
    }
}

/// `isUTF8` (encodedword.go:190).
fn is_utf8(charset: &str) -> bool {
    charset.eq_ignore_ascii_case("UTF-8")
}

/// Why a word would not decode — `WordDecoder.Decode`'s errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// `errInvalidWord` (encodedword.go:25).
    #[error("mime: invalid RFC 2047 encoded-word")]
    InvalidWord,
    /// `fromHex` (encodedword.go:420).
    /// Go's `%#02x` pads the digits to two (`0x05`); Rust's width counts the `0x`, hence `04`.
    #[error("mime: invalid hex byte {0:#04x}")]
    InvalidHexByte(u8),
    /// A `B` word whose base64 does not decode.
    #[error(transparent)]
    Base64(#[from] base64::CorruptInputError),
    /// The charset is neither UTF-8, ISO-8859-1 nor US-ASCII. `net/mail` installs a
    /// `CharsetReader` that fails with its `charsetError`, `charset not supported: %q`, and
    /// passes the charset **lower-cased** (encodedword.go:318).
    #[error("charset not supported: {}", crate::strconv::quote(.0))]
    CharsetNotSupported(String),
}

/// Port of `WordDecoder.Decode` (encodedword.go:198) with the `CharsetReader` `net/mail`
/// installs — so an unknown charset is [`DecodeError::CharsetNotSupported`].
pub fn decode_word(word: &str) -> Result<String, DecodeError> {
    if word.len() < 8
        || !word.starts_with("=?")
        || !word.ends_with("?=")
        || word.matches('?').count() != 4
    {
        return Err(DecodeError::InvalidWord);
    }
    let word = &word[2..word.len() - 2];
    let (charset, text) = word.split_once('?').unwrap_or((word, ""));
    if charset.is_empty() {
        return Err(DecodeError::InvalidWord);
    }
    let (encoding, text) = text.split_once('?').unwrap_or((text, ""));
    if encoding.len() != 1 {
        return Err(DecodeError::InvalidWord);
    }
    let content = decode(encoding.as_bytes()[0], text)?;
    convert(charset, &content)
}

/// `decode` (encodedword.go:292).
fn decode(encoding: u8, text: &str) -> Result<Vec<u8>, DecodeError> {
    match encoding {
        b'B' | b'b' => Ok(base64::decode(text)?),
        b'Q' | b'q' => q_decode(text),
        _ => Err(DecodeError::InvalidWord),
    }
}

/// `convert` (encodedword.go:303).
fn convert(charset: &str, content: &[u8]) -> Result<String, DecodeError> {
    if charset.eq_ignore_ascii_case("utf-8") {
        // Go writes the bytes through unchanged; a Rust `String` cannot hold invalid UTF-8, so
        // a bad sequence becomes U+FFFD — the one place this port cannot be byte-exact.
        Ok(String::from_utf8_lossy(content).into_owned())
    } else if charset.eq_ignore_ascii_case("iso-8859-1") {
        Ok(content.iter().map(|&c| char::from(c)).collect())
    } else if charset.eq_ignore_ascii_case("us-ascii") {
        Ok(content
            .iter()
            .map(|&c| {
                if c >= 0x80 {
                    char::REPLACEMENT_CHARACTER
                } else {
                    char::from(c)
                }
            })
            .collect())
    } else {
        Err(DecodeError::CharsetNotSupported(go_to_lower_ascii(charset)))
    }
}

/// `strings.ToLower` for the charset names that reach here, which are atoms (ASCII or UTF-8).
fn go_to_lower_ascii(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// `qDecode` (encodedword.go:376).
fn q_decode(s: &str) -> Result<Vec<u8>, DecodeError> {
    let b = s.as_bytes();
    let mut dec = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'_' => dec.push(b' '),
            b'=' => {
                if i + 2 >= b.len() {
                    return Err(DecodeError::InvalidWord);
                }
                dec.push((from_hex(b[i + 1])? << 4) | from_hex(b[i + 2])?);
                i += 2;
            }
            b' '..=b'~' | b'\n' | b'\r' | b'\t' => dec.push(c),
            _ => return Err(DecodeError::InvalidWord),
        }
        i += 1;
    }
    Ok(dec)
}

/// `fromHex` (encodedword.go:410).
fn from_hex(b: u8) -> Result<u8, DecodeError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err(DecodeError::InvalidHexByte(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_matches_go() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../../fixtures/behaviour_mail.json")).unwrap();
        let rows = oracle["word_encode"].as_array().unwrap();
        let mut checked = 0;
        for row in rows {
            let input = row["input"].as_str().unwrap();
            if input.contains('\u{fffd}') {
                // Invalid UTF-8 in Go, which JSON (and a Rust &str) cannot carry.
                continue;
            }
            let charset = row["charset"].as_str().unwrap();
            assert_eq!(
                WordEncoder::Q.encode(charset, input),
                row["q"].as_str().unwrap(),
                "Q {charset} {input:?}"
            );
            assert_eq!(
                WordEncoder::B.encode(charset, input),
                row["b"].as_str().unwrap(),
                "B {charset} {input:?}"
            );
            checked += 1;
        }
        assert!(checked > 60, "{checked}");
    }

    #[test]
    fn decode_word_branches() {
        assert_eq!(decode_word("=?utf-8?q?J=C3=B6rg?=").unwrap(), "Jörg");
        assert_eq!(decode_word("=?UTF-8?B?SsO2cmc=?=").unwrap(), "Jörg");
        assert_eq!(decode_word("=?iso-8859-1?q?J=F6rg?=").unwrap(), "Jörg");
        assert_eq!(decode_word("=?us-ascii?q?J=F6rg?=").unwrap(), "J\u{fffd}rg");
        assert_eq!(decode_word("=?utf-8?q?a_b?=").unwrap(), "a b");
        assert_eq!(
            decode_word("=?x?q?=?").unwrap_err(),
            DecodeError::InvalidWord
        );
        assert_eq!(
            decode_word("=??q?abc?=").unwrap_err(),
            DecodeError::InvalidWord
        );
        assert_eq!(
            decode_word("=?utf-8?qq?abc?=").unwrap_err(),
            DecodeError::InvalidWord
        );
        assert_eq!(
            decode_word("=?utf-8?x?abc?=").unwrap_err(),
            DecodeError::InvalidWord
        );
        assert_eq!(
            decode_word("=?utf-8?q?ab=Z1?=").unwrap_err().to_string(),
            "mime: invalid hex byte 0x5a"
        );
        assert_eq!(
            decode_word("=?utf-8?q?ab=\t1?=").unwrap_err().to_string(),
            "mime: invalid hex byte 0x09"
        );
        assert_eq!(
            decode_word("=?utf-8?q?ab=1?=").unwrap_err(),
            DecodeError::InvalidWord
        );
        assert_eq!(
            decode_word("=?KOI8-R?q?abc?=").unwrap_err().to_string(),
            "charset not supported: \"koi8-r\""
        );
    }
}
