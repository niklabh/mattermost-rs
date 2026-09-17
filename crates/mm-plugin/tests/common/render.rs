//! The canonical rendering of `reference/dump/plugingen`, and fixture loading. Shared by the
//! test suites and by `examples/conformance_plugin.rs`, so it uses no test-only environment.

#![allow(dead_code)]

use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use gobwire::{Decoder, Dynamic, Encode, Encoder, MarshalKind, Progress, Type, Value};
use serde_json::{Map, Value as Json, json};

pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn fixtures() -> PathBuf {
    root().join("fixtures/plugin")
}

/// The first complete value of a stream.
pub fn first_value<T>(
    stream: &[u8],
    f: impl FnOnce(&mut Decoder) -> gobwire::Result<T>,
) -> Result<T, String> {
    let mut dec = Decoder::new();
    let mut rest = stream;
    while !rest.is_empty() {
        let (width, n) = gobwire::parse_length_prefix(rest)
            .map_err(|e| e.to_string())?
            .ok_or("truncated length")?;
        let body = rest.get(width..width + n).ok_or("truncated message")?;
        rest = &rest[width + n..];
        if dec.push_message(body).map_err(|e| e.to_string())? == Progress::Ready {
            return f(&mut dec).map_err(|e| e.to_string());
        }
    }
    Err("stream ended before a value".into())
}

pub fn render_stream(stream: &[u8]) -> Result<Json, String> {
    let d: Dynamic = first_value(stream, |dec| dec.decode())?;
    Ok(render(&d.ty, &d.value, &mut Vec::new()))
}

/// `reference/dump/plugingen`'s rendering (the same as `reference/dump/gob`'s).
pub fn render(
    ty: &Type,
    v: &Value,
    structs: &mut Vec<std::sync::Arc<gobwire::StructType>>,
) -> Json {
    let ty = match ty {
        Type::Ref(n) => &Type::Struct(structs[structs.len() - 1 - n].clone()),
        other => other,
    };
    match (ty, v) {
        (_, Value::Bool(b)) => json!(b),
        (_, Value::Int(i)) => json!(i),
        (_, Value::Uint(u)) => json!(u),
        (_, Value::Float(f)) => json!({ "$f64": f.to_bits() }),
        (_, Value::Complex(re, im)) => json!({ "$c128": [re.to_bits(), im.to_bits()] }),
        (_, Value::Bytes(b)) => json!({ "$bytes": B64.encode(b) }),
        (_, Value::String(s)) => json!(s),
        (_, Value::Interface(None)) => json!({ "$iface": "" }),
        (_, Value::Interface(Some(i))) => {
            json!({ "$iface": i.name, "value": render(&i.ty, &i.value, &mut Vec::new()) })
        }
        (Type::Marshaler(kind, _), Value::Marshaled(b)) => {
            let key = match kind {
                MarshalKind::Gob => "$gob",
                MarshalKind::Binary => "$bin",
                MarshalKind::Text => "$text",
            };
            json!({ key: B64.encode(b) })
        }
        (Type::Slice(t) | Type::Array(t, _), Value::Slice(items) | Value::Array(items)) => {
            Json::Array(items.iter().map(|x| render(t, x, structs)).collect())
        }
        (Type::Map(kt, vt), Value::Map(pairs)) => {
            let mut m = Map::new();
            for (k, v) in pairs {
                let key = match render(kt, k, structs) {
                    Json::String(s) => s,
                    other => other.to_string(),
                };
                m.insert(key, render(vt, v, structs));
            }
            json!({ "$map": m })
        }
        (Type::Struct(st), Value::Struct(fields)) => {
            structs.push(st.clone());
            let mut m = Map::new();
            for ((name, t), v) in st.fields.iter().zip(fields) {
                if let Some(v) = v {
                    m.insert(name.clone(), render(t, v, structs));
                }
            }
            structs.pop();
            Json::Object(m)
        }
        _ => panic!("value {v:?} does not match type {ty:?}"),
    }
}

/// How Go would render a typed value: encode it, read it back dynamically, render.
pub fn render_typed<T: Encode + ?Sized>(value: &T) -> Json {
    let bytes = Encoder::new().encode(value).unwrap();
    render_stream(&bytes).unwrap()
}

/// Decode a fixture stream **into** `seed`, as a merging client does: a field the stream omits
/// keeps the seed's value.
pub fn fixture_into<T: gobwire::Decode + Send + 'static>(name: &str, seed: T) -> T {
    let stream = std::fs::read(fixtures().join(format!("gob/{name}.gob"))).unwrap();
    let mut value = Some(seed);
    first_value(&stream, |dec| {
        let mut taken = value.take().expect("one value");
        dec.decode_into(&mut taken)?;
        value = Some(taken);
        Ok(())
    })
    .unwrap();
    value.expect("one value")
}

/// How a merging client's answer renders: the fixture stream decoded into `seed`.
pub fn merged<T: gobwire::Decode + Encode + Send + 'static>(name: &str, seed: T) -> Json {
    render_typed(&fixture_into(name, seed))
}

/// Decode a fixture stream (`fixtures/plugin/gob/<name>.gob`) into `T`.
pub fn fixture<T: gobwire::Decode + Default + Send + 'static>(name: &str) -> T {
    let stream = std::fs::read(fixtures().join(format!("gob/{name}.gob"))).unwrap();
    first_value(&stream, |dec| dec.decode()).unwrap()
}
