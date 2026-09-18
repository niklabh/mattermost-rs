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

/// Records what a plugin wrote, as the Go host's `httptest.ResponseRecorder` does.
#[derive(Clone, Default)]
pub struct Recorder(pub std::sync::Arc<std::sync::Mutex<Recorded>>);

#[derive(Default)]
pub struct Recorded {
    pub header: mm_plugin::wire::http::Header,
    pub status: Option<i64>,
    pub body: Vec<u8>,
}

impl Recorder {
    /// What was written, in the shape `http_response` describes.
    pub fn response(&self) -> Json {
        let recorded = self.0.lock().unwrap();
        let header: serde_json::Map<String, Json> = recorded
            .header
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        json!({
            "status": recorded.status.unwrap_or(200),
            "header": header,
            "body": String::from_utf8_lossy(&recorded.body),
        })
    }
}

impl mm_plugin::http::ResponseWriter for Recorder {
    fn header(&mut self) -> mm_plugin::wire::http::Header {
        self.0.lock().unwrap().header.clone()
    }

    fn sync_header(&mut self, header: mm_plugin::wire::http::Header) {
        self.0.lock().unwrap().header = header;
    }

    fn write(&mut self, body: &[u8]) -> std::io::Result<()> {
        self.0.lock().unwrap().body.extend_from_slice(body);
        Ok(())
    }

    fn write_header(&mut self, status: i64) {
        let mut recorded = self.0.lock().unwrap();
        // Go's recorder keeps the first status written.
        if recorded.status.is_none() {
            recorded.status = Some(status);
        }
    }
}

/// The request both conformance plugins are asked to serve.
pub fn http_request() -> mm_plugin::wire::plugin::HTTPRequestSubset {
    use std::collections::HashMap;
    mm_plugin::wire::plugin::HTTPRequestSubset {
        method: "POST".into(),
        // `url.URL` crosses as its `MarshalBinary`, which is the URL itself.
        url: Some(gobwire::BinaryBytes(CONFORMANCE_URL.as_bytes().to_vec())),
        proto: "HTTP/1.1".into(),
        proto_major: 1,
        proto_minor: 1,
        header: HashMap::from([
            (
                "X-Request".to_owned(),
                vec!["one".to_owned(), "two".to_owned()],
            ),
            ("Content-Type".to_owned(), vec!["text/plain".to_owned()]),
        ]),
        host: "example.test".into(),
        remote_addr: "10.0.0.1:1234".into(),
        request_uri: CONFORMANCE_URL.into(),
        body: None,
    }
}

/// The URL of that request.
pub const CONFORMANCE_URL: &str = "/plugins/conformance/hello?q=1";

/// Go's `http.NotFound`, which is what an unserved request answers with.
pub fn http_not_found() -> Json {
    json!({
        "status": 404,
        "header": {
            "Content-Type": ["text/plain; charset=utf-8"],
            "X-Content-Type-Options": ["nosniff"],
        },
        "body": "404 page not found\n",
    })
}

/// What a conformance plugin answers it with: one header, a status, and a body naming the number
/// of bytes it read. Both plugins follow this rule, so either side's host can check it.
pub fn http_response(method: &str, url: &str, body: &[u8]) -> Json {
    use sha2::{Digest, Sha256};
    let digest = format!("{:x}", Sha256::digest(body));
    json!({
        "status": 203,
        "header": { "X-Conformance": [format!("{method} {url} {}", &digest[..16])] },
        "body": format!("conformance: {} bytes", body.len()),
    })
}

/// What every conformance stream carries: more than one 32 KiB chunk of io_rpc.go's copy buffer,
/// so the framing is exercised rather than a single read.
pub fn stream_payload() -> Vec<u8> {
    (0..70_000u32).map(|i| ((i * 31 + 7) % 251) as u8).collect()
}

/// How a transcript identifies a stream's contents.
pub fn stream_digest(data: &[u8]) -> Json {
    use sha2::{Digest, Sha256};
    json!({ "len": data.len(), "sha256": format!("{:x}", Sha256::digest(data)) })
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
