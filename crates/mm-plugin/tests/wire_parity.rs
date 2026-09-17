//! Every generated wire struct against Go, in both directions.
//!
//! The oracle is `reference/dump/plugingen gob`: two streams per `Z_*` struct and `expected.json`
//! with Go's rendering of each after decoding it back. The full stream populates every reachable
//! field with a distinctive value (integers near their type's limit, interfaces holding each type
//! the plugin RPC registers); the sparse one leaves about half the pointers, interfaces, slices and
//! maps nil. For each stream this checks, in order:
//!
//! 1. Go's stream read **dynamically** renders as Go rendered it (the oracle and the renderer
//!    agree, and every interface name on the wire is the one Go registered);
//! 2. Go's stream decodes into the **generated type**, which re-encodes to a stream that renders
//!    the same, so no field was dropped, renamed or narrowed;
//! 3. that stream decodes back into an equal value;
//! 4. **Go** decodes every stream Rust wrote and renders it the same (one Go process for all).
//!
//! The drift checks at the bottom fail when the Go tree, the IDL, the fixtures or the generated
//! Rust fall out of step.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use gobwire::{Decode, Decoder, Dynamic, Encode, Encoder, MarshalKind, Progress, Type, Value};
use serde_json::{Map, Value as Json, json};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixtures() -> PathBuf {
    root().join("fixtures/plugin")
}

/// The first complete value of a stream.
fn first_value<T>(
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

fn render_stream(stream: &[u8]) -> Result<Json, String> {
    let d: Dynamic = first_value(stream, |dec| dec.decode())?;
    Ok(render(&d.ty, &d.value, &mut Vec::new()))
}

/// `reference/dump/plugingen`'s rendering (the same as `reference/dump/gob`'s).
fn render(ty: &Type, v: &Value, structs: &mut Vec<std::sync::Arc<gobwire::StructType>>) -> Json {
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

/// Decode Go's stream into `T`, re-encode it, and check the re-encoding decodes back equal.
fn typed_round_trip<T: Decode + Encode + Default + PartialEq + std::fmt::Debug>(
    stream: &[u8],
) -> Result<Vec<u8>, String> {
    let value: T = first_value(stream, |dec| dec.decode())?;
    let bytes = Encoder::new().encode(&value).map_err(|e| e.to_string())?;
    let again: T = first_value(&bytes, |dec| dec.decode())?;
    if again != value {
        return Err(format!(
            "re-decoded value differs:\n{again:?}\n!=\n{value:?}"
        ));
    }
    Ok(bytes)
}

type RoundTrip = fn(&[u8]) -> Result<Vec<u8>, String>;

macro_rules! table {
    ($(($name:literal, $ty:ty),)*) => {
        &[$(($name, (|s| typed_round_trip::<$ty>(s)) as RoundTrip),)*]
    };
}

static WIRE: &[(&str, RoundTrip)] = mm_plugin::for_each_wire_struct!(table);

const VARIANTS: [&str; 2] = ["", ".sparse"];

/// Downcast an interface value into the generated type for its registered name, and re-wrap it.
fn interface_round_trip<T: Decode + Encode + Default>(
    i: &gobwire::Interface,
) -> Result<gobwire::Interface, String> {
    let value: T = i.downcast().map_err(|e| e.to_string())?;
    gobwire::Interface::new(i.name.clone(), &value).map_err(|e| e.to_string())
}

type IfaceRoundTrip = fn(&gobwire::Interface) -> Result<gobwire::Interface, String>;

macro_rules! registered_table {
    ($(($name:expr, $ty:ty),)*) => {
        &[$(($name, (|i| interface_round_trip::<$ty>(i)) as IfaceRoundTrip),)*]
    };
}

static REGISTERED: &[(&str, IfaceRoundTrip)] = mm_plugin::for_each_registered!(registered_table);

/// Every interface value in a dynamic value, outermost first.
fn interfaces<'a>(v: &'a Value, out: &mut Vec<&'a gobwire::Interface>) {
    match v {
        Value::Interface(Some(i)) => {
            out.push(i);
            interfaces(&i.value, out);
        }
        Value::Slice(items) | Value::Array(items) => items.iter().for_each(|x| interfaces(x, out)),
        Value::Map(pairs) => pairs.iter().for_each(|(k, x)| {
            interfaces(k, out);
            interfaces(x, out);
        }),
        Value::Struct(fields) => fields.iter().flatten().for_each(|x| interfaces(x, out)),
        _ => {}
    }
}

/// `reference/dump/plugingen`, built once per test process.
fn plugingen() -> &'static PathBuf {
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

fn run_plugingen(args: &[&std::ffi::OsStr]) -> Vec<u8> {
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

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn expected() -> Map<String, Json> {
    let text = std::fs::read_to_string(fixtures().join("gob/expected.json")).unwrap_or_else(|e| {
        panic!("{e}; regenerate with `cd reference/dump && TZ=Asia/Kolkata go run ./plugingen gob ../../fixtures/plugin/gob`")
    });
    serde_json::from_str(&text).unwrap()
}

fn report(failures: &[String]) {
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

#[test]
fn every_wire_struct_is_in_the_oracle() {
    let expected = expected();
    assert_eq!(
        2 * WIRE.len(),
        expected.len(),
        "generated wire structs vs oracle streams"
    );
    for (name, _) in WIRE {
        for variant in VARIANTS {
            assert!(
                expected.contains_key(&format!("{name}{variant}")),
                "{name}{variant} has no oracle stream"
            );
        }
    }
}

#[test]
fn wire_structs_round_trip_through_rust_and_go() {
    let expected = expected();
    let written = scratch("plugin-wire-rust");
    let mut failures = Vec::new();
    for (name, round_trip) in WIRE
        .iter()
        .flat_map(|(n, rt)| VARIANTS.map(|v| (format!("{n}{v}"), rt)))
    {
        let want = &expected[&name];
        let stream = std::fs::read(fixtures().join(format!("gob/{name}.gob"))).unwrap();
        match render_stream(&stream) {
            Ok(got) if &got == want => {}
            Ok(got) => failures.push(format!(
                "{name}: Go's stream renders\n{got}\nexpected\n{want}"
            )),
            Err(e) => failures.push(format!(
                "{name}: Go's stream does not decode dynamically: {e}"
            )),
        }
        let bytes = match round_trip(&stream) {
            Ok(b) => b,
            Err(e) => {
                failures.push(format!("{name}: typed round trip: {e}"));
                continue;
            }
        };
        match render_stream(&bytes) {
            Ok(got) if &got == want => {}
            Ok(got) => failures.push(format!(
                "{name}: Rust's stream renders\n{got}\nexpected\n{want}"
            )),
            Err(e) => failures.push(format!("{name}: Rust's stream does not decode: {e}")),
        }
        std::fs::write(written.join(format!("{name}.gob")), &bytes).unwrap();
    }
    report(&failures);

    let echoed: Map<String, Json> =
        serde_json::from_slice(&run_plugingen(&["echo".as_ref(), written.as_os_str()])).unwrap();
    assert_eq!(echoed.len(), expected.len(), "Go echoed every stream");
    for (name, got) in &echoed {
        if got != &expected[name] {
            failures.push(format!(
                "{name}: Go decodes Rust's stream as\n{got}\nexpected\n{}",
                expected[name]
            ));
        }
    }
    report(&failures);
}

/// Every name `mm_plugin::wire::registered` declares is one Go put on the wire in the oracle, and
/// every interface name in the oracle is either registered there or one of gob's basic types.
#[test]
fn registered_names_are_what_go_sends() {
    fn names(j: &Json, out: &mut std::collections::BTreeSet<String>) {
        match j {
            Json::Object(m) => {
                if let Some(Json::String(n)) = m.get("$iface") {
                    out.insert(n.clone());
                }
                m.values().for_each(|v| names(v, out));
            }
            Json::Array(a) => a.iter().for_each(|v| names(v, out)),
            _ => {}
        }
    }
    let mut sent = std::collections::BTreeSet::new();
    names(&Json::Object(expected()), &mut sent);
    let registered = mm_plugin::wire::registered::ALL;
    for name in registered {
        assert!(
            sent.contains(*name),
            "{name} is registered but never sent by the oracle"
        );
    }
    let basic = ["", "string", "bool", "float64", "int64"];
    for name in &sent {
        assert!(
            registered.contains(&name.as_str()) || basic.contains(&name.as_str()),
            "Go sent interface type {name:?}, which mm_plugin::wire::registered does not name"
        );
    }
}

/// Interface values are decoded dynamically by the wire structs, so the generated types for the
/// registered names (`ErrorString`, the autocomplete args…) are exercised here: every interface
/// value Go sent under a registered name downcasts into its generated type and re-encodes the same.
#[test]
fn registered_types_round_trip_through_their_generated_types() {
    let mut failures = Vec::new();
    let mut seen = std::collections::BTreeMap::<&str, usize>::new();
    for (name, _) in WIRE {
        for variant in VARIANTS {
            let stream =
                std::fs::read(fixtures().join(format!("gob/{name}{variant}.gob"))).unwrap();
            let d: Dynamic = first_value(&stream, |dec| dec.decode()).unwrap();
            let mut found = Vec::new();
            interfaces(&d.value, &mut found);
            for i in found {
                let Some((reg, round_trip)) = REGISTERED.iter().find(|(n, _)| *n == i.name) else {
                    continue;
                };
                *seen.entry(reg).or_default() += 1;
                let want = render(&i.ty, &i.value, &mut Vec::new());
                match round_trip(i) {
                    Ok(back) => {
                        let got = render(&back.ty, &back.value, &mut Vec::new());
                        if got != want {
                            failures.push(format!(
                                "{name}{variant} {reg}: re-encodes as\n{got}\nexpected\n{want}"
                            ));
                        }
                    }
                    Err(e) => failures.push(format!("{name}{variant} {reg}: {e}")),
                }
            }
        }
    }
    report(&failures);
    for (reg, _) in REGISTERED {
        assert!(seen.contains_key(reg), "no oracle stream sends a {reg}");
    }
}

fn same_tree(a: &Path, b: &Path) -> Vec<String> {
    let mut diffs = Vec::new();
    let names = |d: &Path| {
        let mut v: Vec<_> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    };
    let (na, nb) = (names(a), names(b));
    if na != nb {
        diffs.push(format!("file lists differ: {} vs {}", na.len(), nb.len()));
    }
    for n in na.iter().filter(|n| nb.contains(n)) {
        if std::fs::read(a.join(n)).unwrap() != std::fs::read(b.join(n)).unwrap() {
            diffs.push(format!("{} differs", n.to_string_lossy()));
        }
    }
    diffs
}

/// The IDL and the gob oracle match what the pinned Go tree produces now.
#[test]
fn fixtures_match_the_go_tree() {
    let dir = scratch("plugin-idl");
    let idl = dir.join("idl.json");
    run_plugingen(&["idl".as_ref(), idl.as_os_str()]);
    let fresh: Json = serde_json::from_slice(&std::fs::read(&idl).unwrap()).unwrap();
    let committed: Json =
        serde_json::from_slice(&std::fs::read(fixtures().join("idl.json")).unwrap()).unwrap();
    // The Go version is recorded, not compared: a toolchain upgrade alone is not drift.
    let strip = |mut j: Json| {
        j.as_object_mut().unwrap().remove("go_version");
        j
    };
    assert!(
        strip(fresh) == strip(committed),
        "fixtures/plugin/idl.json is stale: regenerate with `cd reference/dump && TZ=Asia/Kolkata go run ./plugingen idl ../../fixtures/plugin/idl.json`, then scripts/plugingen.py"
    );

    let gob = dir.join("gob");
    run_plugingen(&["gob".as_ref(), gob.as_os_str()]);
    let diffs = same_tree(&gob, &fixtures().join("gob"));
    assert!(diffs.is_empty(), "fixtures/plugin/gob is stale: {diffs:?}");
}

/// The committed Rust is what scripts/plugingen.py generates from the committed IDL.
#[test]
fn generated_rust_is_current() {
    let out = Command::new("python3")
        .arg(root().join("scripts/plugingen.py"))
        .arg("--check")
        .output()
        .unwrap_or_else(|e| panic!("the drift check needs python3 on PATH: {e}"));
    assert!(
        out.status.success(),
        "crates/mm-plugin/src/wire is stale; run scripts/plugingen.py:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
