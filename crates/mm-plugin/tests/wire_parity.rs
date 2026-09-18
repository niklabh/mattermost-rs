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

use std::path::Path;
use std::process::Command;

use gobwire::{Decode, Dynamic, Encode, Encoder, Value};
use serde_json::{Map, Value as Json};

mod common;
use common::*;

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

/// The oracle's streams for one wire struct: both variants, plus the gob-safe form of an audit
/// record's arguments, which `LogAuditRec` sends (audit.go).
fn variants_of(name: &str, expected: &Map<String, Json>) -> Vec<String> {
    let mut out: Vec<String> = VARIANTS.iter().map(|v| format!("{name}{v}")).collect();
    let safe = format!("{name}.safe");
    if expected.contains_key(&safe) {
        out.push(safe);
    }
    out
}

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

#[test]
fn every_wire_struct_is_in_the_oracle() {
    let expected = expected();
    let streams: usize = WIRE
        .iter()
        .map(|(name, _)| variants_of(name, &expected).len())
        .sum();
    assert_eq!(
        streams,
        expected.len(),
        "generated wire structs vs oracle streams"
    );
}

#[test]
fn wire_structs_round_trip_through_rust_and_go() {
    let expected = expected();
    let written = scratch("plugin-wire-rust");
    let mut failures = Vec::new();
    let variants: Vec<(String, &RoundTrip)> = WIRE
        .iter()
        .flat_map(|(n, rt)| variants_of(n, &expected).into_iter().map(move |v| (v, rt)))
        .collect();
    for (name, round_trip) in variants {
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

    // Regenerate **over a copy** of what is committed: a stream carrying a map with more than
    // one key is encoded in Go's randomised map order, so the generator keeps a file that still
    // decodes the same way (`writeStable`). The contract is that a run changes nothing, not that
    // an empty directory comes out byte for byte the same.
    let gob = dir.join("gob");
    std::fs::create_dir_all(&gob).unwrap();
    for entry in std::fs::read_dir(fixtures().join("gob")).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), gob.join(entry.file_name())).unwrap();
    }
    run_plugingen(&["gob".as_ref(), gob.as_os_str()]);
    let diffs = same_tree(&gob, &fixtures().join("gob"));
    assert!(diffs.is_empty(), "fixtures/plugin/gob is stale: {diffs:?}");
}

/// io_rpc's framing is Go's `binary.PutVarint`, byte for byte.
#[test]
fn varints_match_go() {
    let go: Map<String, Json> =
        serde_json::from_slice(&run_plugingen(&["varints".as_ref(), "-".as_ref()])).unwrap();
    assert!(go.len() >= 18, "the corpus shrank: {}", go.len());
    for (value, hex) in &go {
        let value: i64 = value.parse().unwrap();
        let mut buf = [0u8; 10];
        let n = mm_plugin::io_rpc::put_varint(value, &mut buf);
        let encoded = buf[..n]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        assert_eq!(Json::String(encoded), *hex, "PutVarint({value})");
    }
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
