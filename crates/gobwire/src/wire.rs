//! Primitive encodings (encoding/gob/doc.go, "Encoding Details").

use crate::error::{Error, Result};

/// Built-in type ids, predefined on every connection (type.go:282-288, doc.go).
pub mod ids {
    pub const BOOL: i64 = 1;
    pub const INT: i64 = 2;
    pub const UINT: i64 = 3;
    pub const FLOAT: i64 = 4;
    pub const BYTES: i64 = 5;
    pub const STRING: i64 = 6;
    pub const COMPLEX: i64 = 7;
    pub const INTERFACE: i64 = 8;
    /// `doc.go` shows 65 in its worked example; the code grants user ids from 64
    /// (type.go:167, `firstUserId`).
    pub const FIRST_USER: i64 = 64;
}

/// Go's `tooBig` on a 64-bit platform (decoder.go:19): `(1 << 30) << 3`.
pub const MAX_MESSAGE_LEN: u64 = 1 << 33;

/// Append an unsigned integer: below 128 as one byte, otherwise the negated byte count followed
/// by the minimal big-endian bytes.
pub fn put_uint(out: &mut Vec<u8>, u: u64) {
    if u < 0x80 {
        out.push(u as u8);
        return;
    }
    let be = u.to_be_bytes();
    let skip = (u.leading_zeros() / 8) as usize;
    let n = 8 - skip;
    out.push((n as u8).wrapping_neg());
    out.extend_from_slice(&be[skip..]);
}

/// Append a signed integer: bit 0 says whether the rest is complemented.
pub fn put_int(out: &mut Vec<u8>, i: i64) {
    let u = if i < 0 {
        (!(i as u64)) << 1 | 1
    } else {
        (i as u64) << 1
    };
    put_uint(out, u);
}

/// Append a float: the IEEE bits, byte-reversed, as an unsigned integer.
pub fn put_float(out: &mut Vec<u8>, f: f64) {
    put_uint(out, f.to_bits().swap_bytes());
}

/// Append a length-prefixed byte string.
pub fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_uint(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// Decode an unsigned integer from the front of `buf`, returning it and its width.
///
/// `Ok(None)` means `buf` is too short to hold it — the case a framing reader waits on.
pub fn parse_uint(buf: &[u8]) -> Result<Option<(u64, usize)>> {
    let Some(&first) = buf.first() else {
        return Ok(None);
    };
    if first < 0x80 {
        return Ok(Some((u64::from(first), 1)));
    }
    // decode.go, decodeUint: `n := -int(int8(b)); if n > uint64Size { error_(errBadUint) }`.
    let n = usize::from(first.wrapping_neg());
    if n > 8 {
        return Err(Error::BadUint);
    }
    if buf.len() < 1 + n {
        return Ok(None);
    }
    let x = buf[1..=n]
        .iter()
        .fold(0u64, |acc, &b| acc << 8 | u64::from(b));
    Ok(Some((x, 1 + n)))
}

/// Parse a message's length prefix. `Ok(None)` if `buf` does not yet hold the whole prefix;
/// otherwise the prefix width and the body length that follows it.
pub fn parse_length_prefix(buf: &[u8]) -> Result<Option<(usize, usize)>> {
    match parse_uint(buf)? {
        None => Ok(None),
        Some((n, width)) => {
            if n >= MAX_MESSAGE_LEN {
                return Err(Error::Corrupt("invalid message length".into()));
            }
            let n =
                usize::try_from(n).map_err(|_| Error::Corrupt("invalid message length".into()))?;
            Ok(Some((width, n)))
        }
    }
}

/// Frame a message body with its length prefix.
pub fn frame(out: &mut Vec<u8>, body: &[u8]) {
    put_bytes(out, body);
}

pub(crate) fn uint_to_int(u: u64) -> i64 {
    if u & 1 == 1 {
        !((u >> 1) as i64)
    } else {
        (u >> 1) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc_uint(u: u64) -> Vec<u8> {
        let mut v = Vec::new();
        put_uint(&mut v, u);
        v
    }

    fn enc_int(i: i64) -> Vec<u8> {
        let mut v = Vec::new();
        put_int(&mut v, i);
        v
    }

    /// The worked values in doc.go.
    #[test]
    fn doc_examples() {
        assert_eq!(enc_uint(0), [0x00]);
        assert_eq!(enc_uint(7), [0x07]);
        assert_eq!(enc_uint(127), [0x7f]);
        assert_eq!(enc_uint(128), [0xff, 0x80]);
        assert_eq!(enc_uint(256), [0xfe, 0x01, 0x00]);
        assert_eq!(enc_int(-129), [0xfe, 0x01, 0x01]);
        let mut f = Vec::new();
        put_float(&mut f, 17.0);
        assert_eq!(f, [0xfe, 0x31, 0x40]);
    }

    #[test]
    fn uint_round_trip_at_every_width() {
        for shift in 0..64 {
            for delta in [0u64, 1] {
                let u = (1u64 << shift).wrapping_sub(delta);
                let b = enc_uint(u);
                assert_eq!(parse_uint(&b).unwrap(), Some((u, b.len())), "{u}");
            }
        }
        let max = enc_uint(u64::MAX);
        assert_eq!(max.len(), 9);
        assert_eq!(parse_uint(&max).unwrap(), Some((u64::MAX, 9)));
    }

    #[test]
    fn int_extremes() {
        for i in [0, 1, -1, 63, -64, 64, -65, i64::MAX, i64::MIN] {
            let b = enc_int(i);
            let (u, _) = parse_uint(&b).unwrap().unwrap();
            assert_eq!(uint_to_int(u), i);
        }
        assert_eq!(enc_int(i64::MIN).len(), 9);
    }

    #[test]
    fn short_and_bad_uints() {
        assert_eq!(parse_uint(&[]).unwrap(), None);
        assert_eq!(parse_uint(&[0xfe, 0x01]).unwrap(), None);
        // A count of nine bytes is rejected like Go's errBadUint.
        assert!(matches!(
            parse_uint(&[0xf7, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Err(Error::BadUint)
        ));
        // Go accepts a non-minimal encoding.
        assert_eq!(parse_uint(&[0xfe, 0x00, 0x05]).unwrap(), Some((5, 3)));
    }
}
