//! Go's `encoding/json.Marshal`, for the [`Value`] kinds — what `jsValEscaper` embeds in a
//! `<script>` context.
//!
//! Struct fields are named as declared: this model carries no `json:` tags, so a Go struct with
//! tags marshals differently. A Go nil slice or map marshals as `null`; this model has no nil
//! slices or maps, so an empty one is `[]` / `{}`.

use crate::strconv::format_float;
use crate::value::Value;

/// `json.Marshal(v)`; `Err` is the error's text.
pub(crate) fn marshal(v: Option<&Value>) -> Result<String, String> {
    let mut out = String::new();
    encode(&mut out, v)?;
    Ok(out)
}

fn encode(out: &mut String, v: Option<&Value>) -> Result<(), String> {
    let Some(v) = v else {
        out.push_str("null");
        return Ok(());
    };
    match v {
        Value::Nil | Value::NilPtr(_) => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) => out.push_str(&encode_float(*f)?),
        Value::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode(out, Some(item))?;
            }
            out.push(']');
        }
        Value::Map(m) => {
            out.push('{');
            for (i, (k, item)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_string(out, k);
                out.push(':');
                encode(out, Some(item))?;
            }
            out.push('}');
        }
        Value::Struct(_, fields) => {
            out.push('{');
            for (i, (k, item)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_string(out, k);
                out.push(':');
                encode(out, Some(item))?;
            }
            out.push('}');
        }
        Value::Ptr(inner) => encode(out, Some(inner))?,
        other => {
            let s = other.as_go_string().map(|(s, _)| s).unwrap_or("");
            encode_string(out, s);
        }
    }
    Ok(())
}

/// `floatEncoder.encode` (encode.go:571).
fn encode_float(f: f64) -> Result<String, String> {
    if f.is_nan() || f.is_infinite() {
        return Err(format!(
            "json: unsupported value: {}",
            format_float(f, b'g', -1)
        ));
    }
    let abs = f.abs();
    let fmt = if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        b'e'
    } else {
        b'f'
    };
    let mut b = format_float(f, fmt, -1);
    if fmt == b'e' {
        // clean up e-09 to e-9
        let n = b.len();
        let bb = b.as_bytes();
        if n >= 4 && bb[n - 4] == b'e' && bb[n - 3] == b'-' && bb[n - 2] == b'0' {
            let last = bb[n - 1] as char;
            b.truncate(n - 2);
            b.push(last);
        }
    }
    Ok(b)
}

const HEX: &[u8] = b"0123456789abcdef";

/// `appendString` with HTML escaping (encode.go:999).
fn encode_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' | '>' | '&' => push_u00(out, c as u8),
            c if (c as u32) < 0x20 => push_u00(out, c as u8),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_u00(out: &mut String, b: u8) {
    out.push_str("\\u00");
    out.push(HEX[(b >> 4) as usize] as char);
    out.push(HEX[(b & 0xf) as usize] as char);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_and_strings() {
        assert_eq!(encode_float(1e21).unwrap(), "1e+21");
        assert_eq!(encode_float(1e-7).unwrap(), "1e-7");
        assert_eq!(encode_float(0.1).unwrap(), "0.1");
        assert_eq!(encode_float(100.0).unwrap(), "100");
        let mut s = String::new();
        encode_string(&mut s, "a<b>&\"\u{1}\u{2028}é");
        assert_eq!(s, "\"a\\u003cb\\u003e\\u0026\\\"\\u0001\\u2028é\"");
    }

    #[test]
    fn containers() {
        let v = Value::map([
            ("b", Value::List(vec![Value::Int(1), Value::Nil])),
            ("a", Value::NilPtr("*string".into())),
        ]);
        assert_eq!(marshal(Some(&v)).unwrap(), "{\"a\":null,\"b\":[1,null]}");
    }
}
