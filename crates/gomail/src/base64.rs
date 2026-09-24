//! Port of `encoding/base64.StdEncoding` as `net/smtp` uses it: `Encode` for AUTH responses and
//! `DecodeString` for the server's 334 challenges.
//!
//! The decoder is ported rather than borrowed because its **error** reaches the wire: a server
//! that sends a malformed challenge makes `Auth` fail with Go's `CorruptInputError`, whose text
//! names the offset of the offending byte by Go's own rules (the `\r`/`\n` skipping and the
//! padding checks of `decodeQuantum`, base64.go:312).

const ENCODE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Port of `base64.CorruptInputError` (base64.go:301).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("illegal base64 data at input byte {0}")]
pub struct CorruptInputError(pub usize);

/// `StdEncoding.EncodeToString`: padded, no line breaks.
pub fn encode(src: &[u8]) -> String {
    let mut out = String::with_capacity(src.len().div_ceil(3) * 4);
    for chunk in src.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let sextet = |shift: u32| char::from(ENCODE[((n >> shift) & 0x3f) as usize]);
        out.push(sextet(18));
        out.push(sextet(12));
        out.push(if chunk.len() > 1 { sextet(6) } else { '=' });
        out.push(if chunk.len() > 2 { sextet(0) } else { '=' });
    }
    out
}

fn decode_value(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Port of `StdEncoding.DecodeString`. Go's fast paths only ever take a quantum that
/// `decodeQuantum` would decode identically, so the quantum loop alone is the whole behaviour.
pub fn decode(src: &str) -> Result<Vec<u8>, CorruptInputError> {
    let src = src.as_bytes();
    let mut out = Vec::with_capacity(src.len() / 4 * 3);
    let mut si = 0;
    while si < src.len() {
        let (nsi, done) = decode_quantum(&mut out, src, si)?;
        si = nsi;
        if done {
            break;
        }
    }
    Ok(out)
}

/// `decodeQuantum` (base64.go:312). Returns the next input index and whether decoding has
/// finished (padding seen or input exhausted).
fn decode_quantum(
    out: &mut Vec<u8>,
    src: &[u8],
    mut si: usize,
) -> Result<(usize, bool), CorruptInputError> {
    let mut dbuf = [0u8; 4];
    let mut dlen = 4;
    let mut j = 0;
    let mut finished = false;
    while j < 4 {
        if src.len() == si {
            if j == 0 {
                return Ok((si, true));
            }
            // StdEncoding is padded, so a short final quantum is always an error.
            return Err(CorruptInputError(si - j));
        }
        let input = src[si];
        si += 1;
        if let Some(v) = decode_value(input) {
            dbuf[j] = v;
            j += 1;
            continue;
        }
        if input == b'\n' || input == b'\r' {
            continue;
        }
        if input != b'=' {
            return Err(CorruptInputError(si - 1));
        }
        // Padding.
        match j {
            0 | 1 => return Err(CorruptInputError(si - 1)),
            2 => {
                while si < src.len() && (src[si] == b'\n' || src[si] == b'\r') {
                    si += 1;
                }
                if si == src.len() {
                    return Err(CorruptInputError(src.len()));
                }
                if src[si] != b'=' {
                    return Err(CorruptInputError(si - 1));
                }
                si += 1;
            }
            _ => {}
        }
        while si < src.len() && (src[si] == b'\n' || src[si] == b'\r') {
            si += 1;
        }
        dlen = j;
        finished = true;
        if si < src.len() {
            // Go decodes the quantum and then reports trailing garbage.
            push_quantum(out, &dbuf, dlen);
            return Err(CorruptInputError(si));
        }
        break;
    }
    push_quantum(out, &dbuf, dlen);
    Ok((si, finished))
}

fn push_quantum(out: &mut Vec<u8>, dbuf: &[u8; 4], dlen: usize) {
    let val = (u32::from(dbuf[0]) << 18)
        | (u32::from(dbuf[1]) << 12)
        | (u32::from(dbuf[2]) << 6)
        | u32::from(dbuf[3]);
    let bytes = [(val >> 16) as u8, (val >> 8) as u8, val as u8];
    // dlen 4 → 3 bytes, 3 → 2, 2 → 1.
    out.extend_from_slice(&bytes[..dlen.saturating_sub(1)]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_matches_go() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_mail.json")).unwrap();
        let rows = oracle["base64_decode"].as_array().unwrap();
        assert!(rows.len() > 20);
        for row in rows {
            let input = row["input"].as_str().unwrap();
            let got = decode(input);
            match row["error"].as_str() {
                Some(want) => assert_eq!(got.unwrap_err().to_string(), want, "{input:?}"),
                None => assert_eq!(
                    String::from_utf8(got.unwrap()).unwrap(),
                    row["output"].as_str().unwrap(),
                    "{input:?}"
                ),
            }
        }
    }

    #[test]
    fn encode_pads_like_go() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"a"), "YQ==");
        assert_eq!(encode(b"ab"), "YWI=");
        assert_eq!(encode(b"abc"), "YWJj");
        assert_eq!(encode(b"\x00user\x00pass"), "AHVzZXIAcGFzcw==");
        assert_eq!(encode(&[0xfb, 0xff]), "+/8=");
    }
}
