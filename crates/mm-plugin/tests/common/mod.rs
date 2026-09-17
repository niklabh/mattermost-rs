//! Shared by the mm-plugin suites: fixtures, the canonical rendering, and the Go oracle.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

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

/// `reference/dump/plugingen`, built once per test process.
pub fn plugingen() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugingen");
        let status = Command::new("go")
            .args(["build", "-o"])
            .arg(&out)
            .arg("./plugingen")
            .current_dir(root().join("reference/dump"))
            .status()
            .unwrap_or_else(|e| panic!("the Go oracle needs a Go toolchain on PATH: {e}"));
        assert!(status.success(), "building reference/dump/plugingen failed");
        out
    })
}

pub fn run_plugingen(args: &[&std::ffi::OsStr]) -> Vec<u8> {
    let out = Command::new(plugingen())
        .args(args)
        // plugingen reads the Go tree relative to reference/dump.
        .current_dir(root().join("reference/dump"))
        .env("TZ", "Asia/Kolkata")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "plugingen {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

pub fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn expected() -> Map<String, Json> {
    let text = std::fs::read_to_string(fixtures().join("gob/expected.json")).unwrap_or_else(|e| {
        panic!("{e}; regenerate with `cd reference/dump && TZ=Asia/Kolkata go run ./plugingen gob ../../fixtures/plugin/gob`")
    });
    serde_json::from_str(&text).unwrap()
}

pub fn report(failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

/// How Go would render a typed value: encode it, read it back dynamically, render.
pub fn render_typed<T: Encode + ?Sized>(value: &T) -> Json {
    let bytes = Encoder::new().encode(value).unwrap();
    render_stream(&bytes).unwrap()
}

/// Decode a fixture stream (`fixtures/plugin/gob/<name>.gob`) into `T`.
pub fn fixture<T: gobwire::Decode + Default + Send + 'static>(name: &str) -> T {
    let stream = std::fs::read(fixtures().join(format!("gob/{name}.gob"))).unwrap();
    first_value(&stream, |dec| dec.decode()).unwrap()
}
