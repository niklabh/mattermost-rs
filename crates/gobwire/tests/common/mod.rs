//! Shared by the Go parity suites: fixture loading, the canonical rendering, and the Go oracle.

#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use gobwire::{
    Decode, Decoder, Dynamic, Encode, Encoder, MarshalKind, Progress, Type, Value,
    parse_length_prefix,
};
use serde_json::{Map, Value as Json, json};

/// Every test binary that includes this module allocates through a capped allocator.
///
/// The corrupt-input sweep once drove a decoder bug to 116 GB and the kernel's OOM killer took
/// down unrelated processes with it. Past the cap an allocation fails, which aborts **this** test
/// process with "memory allocation failed" — a loud test failure instead of a dead machine.
/// Mutation runs depend on it: a mutant that removes a count guard must not take the host down.
const ALLOC_CAP: usize = 4 << 30;

struct Capped;

static IN_USE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if IN_USE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size() > ALLOC_CAP {
            IN_USE.fetch_sub(layout.size(), Ordering::Relaxed);
            return std::ptr::null_mut();
        }
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        IN_USE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` came from `alloc` above with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Capped = Capped;

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/gob")
}

#[derive(Debug, serde::Deserialize)]
pub struct Case {
    pub name: String,
    pub stream: String,
    pub base: Option<String>,
    pub decode_as: String,
    pub wire: Vec<Json>,
    pub decoded: Vec<Json>,
    pub error: Option<String>,
}

pub fn cases() -> Vec<Case> {
    let path = fixtures().join("cases.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("{}: {e}; regenerate with `cd reference/dump && TZ=Asia/Kolkata go run ./gob gen ../../fixtures/gob`", path.display())
    });
    serde_json::from_str(&text).unwrap()
}

pub fn read(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join(name)).unwrap()
}

/// Split a stream into message bodies.
pub fn messages(mut stream: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    while !stream.is_empty() {
        let (width, n) = parse_length_prefix(stream)
            .unwrap()
            .expect("truncated stream");
        out.push(&stream[width..width + n]);
        stream = &stream[width + n..];
    }
    out
}

/// Decode `count` values (all, if `None`) from a stream, handing each to `f` with the decoder
/// positioned on it.
pub fn for_each_value(
    stream: &[u8],
    mut f: impl FnMut(&mut Decoder) -> gobwire::Result<()>,
) -> gobwire::Result<usize> {
    let mut dec = Decoder::new();
    let mut n = 0;
    for body in messages(stream) {
        if dec.push_message(body)? == Progress::Ready {
            f(&mut dec)?;
            n += 1;
        }
    }
    Ok(n)
}

/// The canonical rendering of `reference/dump/gob` for a dynamic value.
pub fn render(ty: &Type, v: &Value) -> Json {
    render_in(ty, v, &mut Vec::new())
}

fn render_in(ty: &Type, v: &Value, structs: &mut Vec<std::sync::Arc<gobwire::StructType>>) -> Json {
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
            json!({ "$iface": i.name, "value": render(&i.ty, &i.value) })
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
            Json::Array(items.iter().map(|x| render_in(t, x, structs)).collect())
        }
        (Type::Map(kt, vt), Value::Map(pairs)) => {
            let mut m = Map::new();
            for (k, v) in pairs {
                let key = match render_in(kt, k, structs) {
                    Json::String(s) => s,
                    other => other.to_string(),
                };
                m.insert(key, render_in(vt, v, structs));
            }
            json!({ "$map": m })
        }
        (Type::Struct(st), Value::Struct(fields)) => {
            structs.push(st.clone());
            let mut m = Map::new();
            for ((name, t), v) in st.fields.iter().zip(fields) {
                if let Some(v) = v {
                    m.insert(name.clone(), render_in(t, v, structs));
                }
            }
            structs.pop();
            Json::Object(m)
        }
        _ => panic!("value {v:?} does not match type {ty:?}"),
    }
}

/// Render a typed value by encoding it and reading it back dynamically.
pub fn render_typed<T: Encode + ?Sized>(value: &T) -> Json {
    let bytes = Encoder::new().encode(value).unwrap();
    let mut out = None;
    for_each_value(&bytes, |dec| {
        let d: Dynamic = dec.decode()?;
        out = Some(render(&d.ty, &d.value));
        Ok(())
    })
    .unwrap();
    out.expect("no value")
}

/// Decode every value of a case's stream into `T`, as the Go oracle did: a fresh destination
/// per value, or — for a merge case — one destination that first received the base stream.
pub fn decode_typed<T: Decode + Encode + Default>(case: &Case) -> gobwire::Result<Vec<T>> {
    let stream = read(&case.stream);
    let mut out = Vec::new();
    match &case.base {
        Some(base) => {
            let mut dest = T::default();
            for_each_value(&read(base), |dec| dec.decode_into(&mut dest))?;
            for_each_value(&stream, |dec| {
                dec.decode_into(&mut dest)?;
                Ok(())
            })?;
            out.push(dest);
        }
        None => {
            for_each_value(&stream, |dec| {
                out.push(dec.decode::<T>()?);
                Ok(())
            })?;
        }
    }
    Ok(out)
}

/// The Go echo binary, built once per test process.
fn echo_binary() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gobwire-oracle");
        let dump = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../reference/dump");
        let status = Command::new("go")
            .args(["build", "-o"])
            .arg(&out)
            .arg("./gob")
            .current_dir(&dump)
            .status()
            .unwrap_or_else(|e| panic!("the Go oracle needs a Go toolchain on PATH: {e}"));
        assert!(status.success(), "building reference/dump/gob failed");
        out
    })
}

#[derive(Debug, serde::Deserialize)]
pub struct Echo {
    pub decoded: Vec<Json>,
    pub error: Option<String>,
}

/// Ask Go to decode a stream Rust wrote.
pub fn go_echo(decode_as: &str, stream: &[u8], base: Option<&str>) -> Echo {
    let mut cmd = Command::new(echo_binary());
    cmd.arg("echo").arg(decode_as);
    if let Some(base) = base {
        cmd.arg(fixtures().join(base));
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stream).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "go echo failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
