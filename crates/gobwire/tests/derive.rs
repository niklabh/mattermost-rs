//! `#[derive(Gob)]`: names, skipping, transparency, generics.

mod common;

use std::collections::HashMap;

use common::messages;
use gobwire::{Decoder, Encoder, Gob, Progress, WireType};

fn round_trip<T: gobwire::Encode + gobwire::Decode + Default>(v: &T) -> (T, Decoder) {
    let stream = Encoder::new().encode(v).unwrap();
    let mut dec = Decoder::new();
    for b in messages(&stream) {
        if dec.push_message(b).unwrap() == Progress::Ready {
            let out = dec.decode().unwrap();
            return (out, dec);
        }
    }
    panic!("no value");
}

#[derive(Gob, Debug, Default, Clone, PartialEq)]
#[gob(name = "Renamed")]
struct Names {
    request_id: String,
    #[gob(name = "IPAddress")]
    ip_address: String,
    r#type: String,
    #[gob(skip)]
    local_only: u32,
    last: i64,
}

#[test]
fn field_and_type_names() {
    let v = Names {
        request_id: "r".into(),
        ip_address: "10.0.0.1".into(),
        r#type: "t".into(),
        local_only: 7,
        last: 1,
    };
    let (back, dec) = round_trip(&v);
    assert_eq!(back, Names { local_only: 0, ..v });
    let wt = dec.types().get(65).expect("struct defined as 65");
    match &**wt {
        WireType::Struct { name, fields } => {
            assert_eq!(name, "Renamed");
            let names: Vec<_> = fields.iter().map(|f| f.name.as_str()).collect();
            assert_eq!(names, ["RequestId", "IPAddress", "Type", "Last"]);
        }
        other => panic!("{other:?}"),
    }
}

/// Go matches fields by name, so a skipped local field is untouched by a decode.
#[test]
fn skipped_fields_keep_their_value_when_decoding_into() {
    let stream = Encoder::new()
        .encode(&Names {
            last: 5,
            ..Default::default()
        })
        .unwrap();
    let mut dest = Names {
        local_only: 42,
        request_id: "keep".into(),
        ..Default::default()
    };
    let mut dec = Decoder::new();
    for b in messages(&stream) {
        if dec.push_message(b).unwrap() == Progress::Ready {
            dec.decode_into(&mut dest).unwrap();
        }
    }
    assert_eq!(dest.local_only, 42);
    assert_eq!(
        dest.request_id, "keep",
        "an omitted zero field leaves the destination alone"
    );
    assert_eq!(dest.last, 5);
}

#[derive(Gob, Debug, Default, Clone, PartialEq)]
#[gob(transparent)]
struct StringMap(HashMap<String, String>);

#[derive(Gob, Debug, Default, Clone, PartialEq)]
#[gob(transparent)]
struct Named {
    inner: Vec<String>,
}

#[derive(Gob, Debug, Default, Clone, PartialEq)]
struct HasTransparent {
    props: StringMap,
    ids: Named,
}

#[test]
fn transparent_newtypes_are_their_field_on_the_wire() {
    let v = HasTransparent {
        props: StringMap(HashMap::from([("a".into(), "b".into())])),
        ids: Named {
            inner: vec!["x".into()],
        },
    };
    let (back, dec) = round_trip(&v);
    assert_eq!(back, v);
    // No wire type is defined for the newtypes themselves.
    let kinds: Vec<_> = (64..70)
        .filter_map(|i| dec.types().get(i))
        .map(|t| std::mem::discriminant(&**t))
        .collect();
    assert_eq!(kinds.len(), 3, "struct, map and slice only");

    // A zero newtype is omitted exactly as its field would be.
    let (empty, _) = round_trip(&HasTransparent::default());
    assert_eq!(empty, HasTransparent::default());
}

#[derive(Gob, Debug, Default, Clone, PartialEq)]
struct Generic<T> {
    value: T,
    list: Vec<T>,
}

#[test]
fn generic_structs() {
    let v = Generic {
        value: 3u16,
        list: vec![1, 2],
    };
    assert_eq!(round_trip(&v).0, v);
    let s = Generic {
        value: "s".to_string(),
        list: vec![],
    };
    assert_eq!(round_trip(&s).0, s);
}

#[derive(Gob, Debug, Default, Clone, PartialEq)]
struct Unit;

#[derive(Gob, Debug, Default, Clone, PartialEq)]
struct Empty {}

#[test]
fn empty_structs() {
    assert_eq!(round_trip(&Unit).0, Unit);
    assert_eq!(round_trip(&Empty {}).0, Empty {});
    // Go's net/rpc sends `struct{}{}` args as a struct with no fields: id, then the terminator.
    let stream = Encoder::new().encode(&Empty {}).unwrap();
    assert_eq!(messages(&stream)[1], [0xff, 0x82, 0x00]);
}
