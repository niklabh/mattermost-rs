//! Port of encode.go: parameters to the bytes lib/pq sends, and result bytes to [`Value`]s.

use crate::error::Error;
use crate::value::Value;

pub(crate) const T_BOOL: u32 = 16;
pub(crate) const T_BYTEA: u32 = 17;
pub(crate) const T_CHAR: u32 = 18;
pub(crate) const T_INT8: u32 = 20;
pub(crate) const T_INT2: u32 = 21;
pub(crate) const T_INT4: u32 = 23;
pub(crate) const T_TEXT: u32 = 25;
pub(crate) const T_FLOAT4: u32 = 700;
pub(crate) const T_FLOAT8: u32 = 701;
pub(crate) const T_UNKNOWN: u32 = 705;
pub(crate) const T_BPCHAR: u32 = 1042;
pub(crate) const T_VARCHAR: u32 = 1043;
pub(crate) const T_DATE: u32 = 1082;
pub(crate) const T_TIME: u32 = 1083;
pub(crate) const T_TIMESTAMP: u32 = 1114;
pub(crate) const T_TIMESTAMPTZ: u32 = 1184;
pub(crate) const T_TIMETZ: u32 = 1266;
pub(crate) const T_NUMERIC: u32 = 1700;
pub(crate) const T__NUMERIC: u32 = 1231;
pub(crate) const T_UUID: u32 = 2950;

/// The format of a result column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Text,
    Binary,
}

/// `strconv.FormatFloat(v, 'f', -1, 64)`: the shortest decimal that reads back as `v`, never an
/// exponent, and Go's spellings of the infinities.
pub(crate) fn format_float(v: f64) -> String {
    if v.is_infinite() {
        return if v > 0.0 { "+Inf" } else { "-Inf" }.to_owned();
    }
    if v.is_nan() {
        return "NaN".to_owned();
    }
    format!("{v}")
}

/// `encodeBytea`: `\x` and lower-case hex.
pub(crate) fn encode_bytea(v: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(2 + v.len() * 2);
    out.extend_from_slice(b"\\x");
    for b in v {
        out.push(HEX[(b >> 4) as usize]);
        out.push(HEX[(b & 15) as usize]);
    }
    out
}

/// `encode(x, oid)`: `None` is Go's nil result, which the caller sends as `NULL` — a nil
/// `[]byte`, or `nil` itself.
pub(crate) fn encode(x: &Value, oid: u32) -> Result<Option<Vec<u8>>, Error> {
    Ok(Some(match x {
        Value::Null => return Ok(None),
        Value::Int64(v) => v.to_string().into_bytes(),
        Value::Float64(v) => format_float(*v).into_bytes(),
        Value::Bytes(v) => {
            if v.is_empty() {
                return Ok(None);
            }
            if oid == T_BYTEA {
                encode_bytea(v)
            } else {
                v.clone()
            }
        }
        Value::String(v) => {
            if oid == T_BYTEA {
                encode_bytea(v.as_bytes())
            } else {
                v.clone().into_bytes()
            }
        }
        Value::Bool(v) => if *v { "true" } else { "false" }.as_bytes().to_vec(),
        Value::Time(text) => text.clone().into_bytes(),
        Value::Other(name) => {
            return Err(Error::msg(format!("pq: encode: unknown type for {name}")));
        }
    }))
}

/// `binaryEncode`: a `[]byte` as it is, anything else as `encode` with no type.
pub(crate) fn binary_encode(x: &Value) -> Result<Option<Vec<u8>>, Error> {
    match x {
        Value::Bytes(v) => Ok(Some(v.clone())),
        other => encode(other, T_UNKNOWN),
    }
}

/// `decode`: one column of a `DataRow` in its format.
pub(crate) fn decode(s: &[u8], typ: u32, f: Format) -> Result<Value, Error> {
    match f {
        Format::Binary => binary_decode(s, typ),
        Format::Text => text_decode(s, typ),
    }
}

fn be_int(s: &[u8], n: usize) -> Result<i64, Error> {
    let bytes = s
        .get(..n)
        .ok_or_else(|| Error::msg("pq: invalid message format; short binary value"))?;
    let mut v: i64 = 0;
    for b in bytes {
        v = (v << 8) | i64::from(*b);
    }
    Ok(match n {
        2 => i64::from(v as u16 as i16),
        4 => i64::from(v as u32 as i32),
        _ => v,
    })
}

/// `binaryDecode`: the five types lib/pq asks for in binary.
fn binary_decode(s: &[u8], typ: u32) -> Result<Value, Error> {
    match typ {
        T_BYTEA => Ok(Value::Bytes(s.to_vec())),
        T_INT8 => be_int(s, 8).map(Value::Int64),
        T_INT4 => be_int(s, 4).map(Value::Int64),
        T_INT2 => be_int(s, 2).map(Value::Int64),
        T_UUID => decode_uuid_binary(s).map(Value::Bytes),
        _ => Err(Error::msg(format!(
            "pq: don't know how to decode binary parameter of type {typ}"
        ))),
    }
}

/// `decodeUUIDBinary`: the 36-character text form.
fn decode_uuid_binary(src: &[u8]) -> Result<Vec<u8>, Error> {
    if src.len() != 16 {
        return Err(Error::msg(format!(
            "pq: unable to decode uuid; bad length: {}",
            src.len()
        )));
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(36);
    for (i, b) in src.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push(b'-');
        }
        out.push(HEX[(b >> 4) as usize]);
        out.push(HEX[(b & 15) as usize]);
    }
    Ok(out)
}

/// `textDecode`: strings for the character types, the timestamps' infinities as bytes, and
/// every type lib/pq does not name as its raw text.
fn text_decode(s: &[u8], typ: u32) -> Result<Value, Error> {
    Ok(match typ {
        T_CHAR | T_BPCHAR | T_VARCHAR | T_TEXT => {
            Value::String(String::from_utf8_lossy(s).into_owned())
        }
        T_BYTEA => Value::Bytes(parse_bytea(s).map_err(|e| Error::msg(format!("pq: {e}")))?),
        T_TIMESTAMPTZ | T_TIMESTAMP | T_DATE => match s {
            // `parseTS` with `EnableInfinityTs` never called: the text, as bytes.
            b"-infinity" | b"infinity" => Value::Bytes(s.to_vec()),
            _ => Value::Time(String::from_utf8_lossy(s).into_owned()),
        },
        T_TIME | T_TIMETZ => Value::Time(String::from_utf8_lossy(s).into_owned()),
        T_BOOL => Value::Bool(s.first() == Some(&b't')),
        T_INT8 | T_INT4 | T_INT2 => {
            let text = String::from_utf8_lossy(s);
            Value::Int64(text.parse::<i64>().map_err(|_| {
                Error::msg(format!(
                    "pq: strconv.ParseInt: parsing {text:?}: invalid syntax"
                ))
            })?)
        }
        T_FLOAT4 | T_FLOAT8 => {
            let text = String::from_utf8_lossy(s);
            Value::Float64(text.parse::<f64>().map_err(|_| {
                Error::msg(format!(
                    "pq: strconv.ParseFloat: parsing {text:?}: invalid syntax"
                ))
            })?)
        }
        _ => Value::Bytes(s.to_vec()),
    })
}

/// `parseBytea`: the `\x` hex form, or the escape form of servers before 9.0.
fn parse_bytea(s: &[u8]) -> Result<Vec<u8>, String> {
    if let Some(hex) = s.strip_prefix(b"\\x") {
        let nibble = |c: u8| -> Result<u8, String> {
            match c {
                b'0'..=b'9' => Ok(c - b'0'),
                b'a'..=b'f' => Ok(c - b'a' + 10),
                b'A'..=b'F' => Ok(c - b'A' + 10),
                _ => Err(format!("encoding/hex: invalid byte: {:?}", char::from(c))),
            }
        };
        let mut out = Vec::with_capacity(hex.len() / 2);
        for pair in hex.chunks(2) {
            if pair.len() < 2 {
                return Err("encoding/hex: odd length hex string".into());
            }
            out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
        }
        return Ok(out);
    }
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        if rest[0] == b'\\' {
            if rest.len() >= 2 && rest[1] == b'\\' {
                out.push(b'\\');
                rest = &rest[2..];
                continue;
            }
            if rest.len() < 4 {
                return Err(format!("invalid bytea sequence {rest:?}"));
            }
            let octal = std::str::from_utf8(&rest[1..4]).unwrap_or("");
            let r = u8::from_str_radix(octal, 8)
                .map_err(|_| format!("could not parse bytea value: {octal}"))?;
            out.push(r);
            rest = &rest[4..];
        } else {
            match rest.iter().position(|&b| b == b'\\') {
                None => {
                    out.extend_from_slice(rest);
                    break;
                }
                Some(i) => {
                    out.extend_from_slice(&rest[..i]);
                    rest = &rest[i..];
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_are_shortest_and_never_exponential() {
        assert_eq!(format_float(1.5), "1.5");
        assert_eq!(format_float(1e21), "1000000000000000000000");
        assert_eq!(format_float(1e-7), "0.0000001");
        assert_eq!(format_float(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(format_float(f64::INFINITY), "+Inf");
        assert_eq!(format_float(f64::NEG_INFINITY), "-Inf");
        assert_eq!(format_float(-0.0), "-0");
    }

    #[test]
    fn a_bytea_parameter_is_hex_and_an_empty_bytes_is_null() {
        assert_eq!(
            encode(&Value::Bytes(vec![0, 255]), T_BYTEA).unwrap(),
            Some(b"\\x00ff".to_vec())
        );
        assert_eq!(
            encode(&Value::String("ab".into()), T_BYTEA).unwrap(),
            Some(b"\\x6162".to_vec())
        );
        assert_eq!(
            encode(&Value::Bytes(b"raw".to_vec()), T_TEXT).unwrap(),
            Some(b"raw".to_vec())
        );
        assert_eq!(encode(&Value::Bytes(vec![]), T_TEXT).unwrap(), None);
        assert_eq!(encode(&Value::Null, T_TEXT).unwrap(), None);
        assert_eq!(
            encode(&Value::String(String::new()), T_TEXT).unwrap(),
            Some(vec![])
        );
        assert_eq!(
            encode(&Value::Other("int".into()), T_TEXT).unwrap_err(),
            Error::msg("pq: encode: unknown type for int")
        );
    }

    #[test]
    fn text_and_binary_decoding() {
        assert_eq!(
            decode(b"42", T_INT4, Format::Text).unwrap(),
            Value::Int64(42)
        );
        assert_eq!(
            decode(&[0xff, 0xfe], T_INT2, Format::Binary).unwrap(),
            Value::Int64(-2)
        );
        assert_eq!(
            decode(b"\\x6869", T_BYTEA, Format::Text).unwrap(),
            Value::Bytes(b"hi".to_vec())
        );
        assert_eq!(
            decode(b"h\\\\i\\001", T_BYTEA, Format::Text).unwrap(),
            Value::Bytes(b"h\\i\x01".to_vec())
        );
        assert_eq!(
            decode(b"infinity", T_TIMESTAMPTZ, Format::Text).unwrap(),
            Value::Bytes(b"infinity".to_vec())
        );
        assert_eq!(
            decode(b"f", T_BOOL, Format::Text).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            decode(b"{1,2}", 1007, Format::Text).unwrap(),
            Value::Bytes(b"{1,2}".to_vec())
        );
        assert_eq!(
            decode(&[0u8; 16], T_UUID, Format::Binary).unwrap(),
            Value::Bytes(b"00000000-0000-0000-0000-000000000000".to_vec())
        );
    }
}
