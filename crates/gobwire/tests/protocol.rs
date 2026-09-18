//! Decoder and encoder behaviour that needs no Go process: Go's own worked example byte for
//! byte, the push/scan protocol, error branches, and hostile input.

mod common;

use std::collections::HashMap;

use common::messages;
use gobwire::{
    Decoder, Dynamic, Encoder, Error, Gob, Interface, Progress, StreamDecoder, StreamEncoder, ids,
};

#[derive(Gob, Debug, Default, Clone, PartialEq)]
struct Point {
    #[gob(name = "X")]
    x: i64,
    #[gob(name = "Y")]
    y: i64,
}

#[derive(Gob, Debug, Default, Clone, PartialEq)]
struct Inner {
    n: i64,
    s: String,
}

#[derive(Gob, Debug, Default, Clone, PartialEq)]
struct Chain {
    next: Option<Box<Chain>>,
    n: i64,
}

fn decode_all<T: gobwire::Decode + Default>(stream: &[u8]) -> gobwire::Result<Vec<T>> {
    let mut dec = Decoder::new();
    let mut out = Vec::new();
    for body in messages(stream) {
        if dec.push_message(body)? == Progress::Ready {
            out.push(dec.decode()?);
        }
    }
    Ok(out)
}

/// encoding/gob/doc.go: "Given type Point struct {X, Y int} and the value p := Point{22, 33},
/// the bytes transmitted that encode p will be: ...". The first user type id is 65 there, and so
/// is this encoder's.
#[test]
fn doc_go_point_example_byte_for_byte() {
    let want: &[u8] = &[
        0x1f, 0xff, 0x81, 0x03, 0x01, 0x01, 0x05, 0x50, 0x6f, 0x69, 0x6e, 0x74, 0x01, 0xff, 0x82,
        0x00, 0x01, 0x02, 0x01, 0x01, 0x58, 0x01, 0x04, 0x00, 0x01, 0x01, 0x59, 0x01, 0x04, 0x00,
        0x00, 0x00, 0x07, 0xff, 0x82, 0x01, 0x2c, 0x01, 0x42, 0x00,
    ];
    let got = Encoder::new().encode(&Point { x: 22, y: 33 }).unwrap();
    assert_eq!(got, want);
    assert_eq!(
        decode_all::<Point>(want).unwrap(),
        vec![Point { x: 22, y: 33 }]
    );
}

#[test]
fn a_type_is_defined_once_per_stream() {
    let mut enc = Encoder::new();
    let first = enc.encode(&Point { x: 1, y: 2 }).unwrap();
    let second = enc.encode(&Point { x: 3, y: 4 }).unwrap();
    assert_eq!(messages(&first).len(), 2);
    assert_eq!(messages(&second).len(), 1);
    let mut stream = first;
    stream.extend(second);
    assert_eq!(
        decode_all::<Point>(&stream).unwrap(),
        vec![Point { x: 1, y: 2 }, Point { x: 3, y: 4 }]
    );
}

#[test]
fn push_reports_each_stage() {
    let stream = Encoder::new().encode(&Point { x: 5, y: 0 }).unwrap();
    let bodies = messages(&stream);
    let mut dec = Decoder::new();
    assert_eq!(
        dec.push_message(bodies[0]).unwrap(),
        Progress::TypeDefinition(65)
    );
    // Decoding before a value is buffered is refused.
    assert!(dec.decode::<Point>().is_err());
    assert_eq!(dec.push_message(bodies[1]).unwrap(), Progress::Ready);
    // So is pushing past a buffered value.
    assert!(dec.push_message(bodies[1]).is_err());
    assert_eq!(dec.decode::<Point>().unwrap(), Point { x: 5, y: 0 });
    // Types persist: the same value message decodes again.
    assert_eq!(dec.push_message(bodies[1]).unwrap(), Progress::Ready);
    dec.discard().unwrap();
}

/// A Go stream whose value spans messages: the first push of the value asks for more.
#[test]
fn a_value_split_across_messages_needs_more_until_complete() {
    let stream = common::read("ifaces.gob");
    let bodies = messages(&stream);
    let mut dec = Decoder::new();
    let progress: Vec<_> = bodies
        .iter()
        .map(|b| dec.push_message(b).unwrap())
        .collect();
    assert_eq!(progress.first(), Some(&Progress::TypeDefinition(82)));
    assert_eq!(progress.last(), Some(&Progress::Ready));
    let middle = &progress[1..progress.len() - 1];
    assert!(!middle.is_empty() && middle.iter().all(|p| *p == Progress::NeedMore));
    let d: Dynamic = dec.decode().unwrap();
    assert!(d.ty.is_struct());
}

#[test]
fn type_definition_errors() {
    let stream = Encoder::new().encode(&Point::default()).unwrap();
    let def = messages(&stream)[0].to_vec();

    let mut dec = Decoder::new();
    dec.push_message(&def).unwrap();
    assert!(matches!(
        dec.push_message(&def),
        Err(Error::DuplicateType(65))
    ));

    let mut extra = def.clone();
    extra.push(0);
    assert!(
        matches!(Decoder::new().push_message(&extra), Err(Error::Corrupt(m)) if m == "extra data in buffer")
    );

    // Ids below 64 are reserved (decoder.go, recvType).
    let mut low = def.clone();
    low[0..2].copy_from_slice(&[0xff, 0x7f]); // -64 → id 64 is allowed…
    assert!(Decoder::new().push_message(&low).is_ok());
    let mut reserved = def;
    reserved.splice(0..2, [0x7d]); // -63
    assert!(matches!(
        Decoder::new().push_message(&reserved),
        Err(Error::DuplicateType(63))
    ));
}

#[test]
fn value_errors() {
    // A value of a type never defined.
    let body = [0xff, 0xc6, 0x00]; // int(99), then a singleton delta
    assert!(matches!(
        Decoder::new().push_message(&body),
        Err(Error::UndefinedType(99))
    ));

    // A singleton whose delta is not zero.
    let body = [0x0c, 0x01, 0x01, b'x'];
    assert!(matches!(
        Decoder::new().push_message(&body),
        Err(Error::Corrupt(_))
    ));

    // Truncated inside a value (not at an interface boundary): an error, not a wait.
    let stream = Encoder::new()
        .encode(&Inner {
            n: 1,
            s: "long string".into(),
        })
        .unwrap();
    let bodies = messages(&stream);
    let mut dec = Decoder::new();
    dec.push_message(bodies[0]).unwrap();
    let cut = &bodies[1][..bodies[1].len() - 4];
    assert!(matches!(
        dec.push_message(cut),
        Err(Error::Corrupt(_) | Error::UnexpectedEof)
    ));
}

#[test]
fn incompatible_top_level_types_are_refused() {
    let stream = Encoder::new().encode("text").unwrap();
    let mut dec = Decoder::new();
    for b in messages(&stream) {
        dec.push_message(b).unwrap();
    }
    assert!(matches!(dec.decode::<i64>(), Err(Error::TypeMismatch(_))));

    // A local interface only takes a remote interface.
    let stream = Encoder::new().encode("text").unwrap();
    let mut dec = Decoder::new();
    for b in messages(&stream) {
        dec.push_message(b).unwrap();
    }
    assert!(matches!(
        dec.decode::<Option<Interface>>(),
        Err(Error::TypeMismatch(_))
    ));
}

#[test]
fn nil_pointers_cannot_be_encoded_where_go_panics() {
    assert!(matches!(
        Encoder::new().encode(&None::<Inner>),
        Err(Error::Encode(_))
    ));
    assert!(matches!(
        Encoder::new().encode(&vec![None::<Inner>]),
        Err(Error::Encode(_))
    ));
    let map: HashMap<String, Option<Inner>> = HashMap::from([("k".into(), None)]);
    assert!(matches!(Encoder::new().encode(&map), Err(Error::Encode(_))));
}

/// A failed encode leaves no half-defined types behind: the next value re-sends them.
#[test]
fn a_failed_encode_rolls_back_type_definitions() {
    let mut enc = Encoder::new();
    assert!(enc.encode(&vec![Some(Inner::default()), None]).is_err());
    let stream = enc
        .encode(&vec![Some(Inner {
            n: 1,
            s: "a".into(),
        })])
        .unwrap();
    assert_eq!(
        decode_all::<Vec<Option<Inner>>>(&stream).unwrap(),
        vec![vec![Some(Inner {
            n: 1,
            s: "a".into()
        })]]
    );
}

#[test]
fn hostile_counts_do_not_allocate_or_loop() {
    // A []int claiming 2^40 elements with one byte behind it.
    let mut enc = Encoder::new();
    let stream = enc.encode(&vec![1i64]).unwrap();
    let bodies = messages(&stream);
    let mut value = bodies[1].to_vec();
    // id (two bytes), singleton delta, count(1), element(2): replace the count with 2^40.
    let count_at = 3;
    assert_eq!(value[count_at], 1);
    value.splice(count_at..=count_at, [0xfb, 0x01, 0x00, 0x00, 0x00, 0x00]);
    let mut dec = Decoder::new();
    dec.push_message(bodies[0]).unwrap();
    assert!(dec.push_message(&value).is_err());

    // A string claiming more bytes than the message holds.
    let body = [0x0c, 0x00, 0xfe, 0xff, 0xff, b'x'];
    assert!(Decoder::new().push_message(&body).is_err());

    // A message length at Go's tooBig limit (2^33).
    assert!(gobwire::parse_length_prefix(&[0xfb, 0x02, 0x00, 0x00, 0x00, 0x00]).is_err());
}

#[test]
fn nesting_beyond_the_depth_limit_is_refused() {
    let mut chain = Chain { next: None, n: 0 };
    for n in 1..(gobwire::MAX_DEPTH as i64 + 10) {
        chain = Chain {
            next: Some(Box::new(chain)),
            n,
        };
    }
    let stream = Encoder::new().encode(&chain).unwrap();
    let err = decode_all::<Chain>(&stream).unwrap_err();
    assert!(matches!(err, Error::Corrupt(m) if m.contains("nesting depth")));

    let mut shallow = Chain { next: None, n: 0 };
    for n in 1..100 {
        shallow = Chain {
            next: Some(Box::new(shallow)),
            n,
        };
    }
    let stream = Encoder::new().encode(&shallow).unwrap();
    assert_eq!(decode_all::<Chain>(&stream).unwrap(), vec![shallow]);
}

#[test]
fn stream_decoder_end_of_input() {
    let mut empty = StreamDecoder::new(&b""[..]);
    assert!(empty.decode::<Point>().unwrap().is_none());

    let stream = Encoder::new().encode(&Point { x: 1, y: 1 }).unwrap();
    let def_only = &stream[..messages(&stream)[0].len() + 1];
    let mut dangling = StreamDecoder::new(def_only);
    assert!(matches!(
        dangling.decode::<Point>(),
        Err(Error::UnexpectedEof)
    ));

    let mut out = StreamEncoder::new(Vec::new());
    out.encode(&Point { x: 1, y: 2 }).unwrap();
    out.encode(&Point { x: 3, y: 4 }).unwrap();
    let bytes = out.into_inner();
    let mut dec = StreamDecoder::new(&bytes[..]);
    assert_eq!(dec.decode::<Point>().unwrap(), Some(Point { x: 1, y: 2 }));
    assert_eq!(dec.decode::<Point>().unwrap(), Some(Point { x: 3, y: 4 }));
    assert_eq!(dec.decode::<Point>().unwrap(), None);
}

#[test]
fn interface_wrap_and_downcast() {
    let iface = Interface::new(
        "*main.Inner",
        &Inner {
            n: 7,
            s: "seven".into(),
        },
    )
    .unwrap();
    assert_eq!(iface.name, "*main.Inner");
    assert_eq!(
        iface.downcast::<Inner>().unwrap(),
        Inner {
            n: 7,
            s: "seven".into()
        }
    );
    assert!(iface.downcast::<i64>().is_err());

    #[derive(Gob, Default, Debug, PartialEq)]
    struct Holder {
        #[gob(name = "Any")]
        any: Option<Interface>,
    }
    let stream = Encoder::new()
        .encode(&Holder {
            any: Some(iface.clone()),
        })
        .unwrap();
    let back = decode_all::<Holder>(&stream).unwrap().remove(0);
    assert_eq!(
        back.any.unwrap().downcast::<Inner>().unwrap(),
        Inner {
            n: 7,
            s: "seven".into()
        }
    );

    assert!(matches!(
        Encoder::new().encode(&Holder {
            any: Some(Interface {
                name: String::new(),
                ..Interface::int(1)
            })
        }),
        Err(Error::Encode(_))
    ));
    assert_eq!(ids::INTERFACE, 8);
}

/// A field number equal to the field count is out of range (decode.go, decodeStruct: errRange),
/// in a typed decode, a skip and a dynamic decode alike — never an index panic.
#[test]
fn a_field_number_past_the_last_field_is_corrupt() {
    let stream = Encoder::new().encode(&Point { x: 1, y: 2 }).unwrap();
    let bodies = messages(&stream);
    // id 65, then delta 3: field index 2 of a two-field struct.
    let value = [0xff, 0x82, 0x03, 0x02, 0x00];
    for mode in ["typed", "dynamic", "discard"] {
        let mut dec = Decoder::new();
        dec.push_message(bodies[0]).unwrap();
        let result = match dec.push_message(&value) {
            Err(e) => Err(e),
            Ok(_) => match mode {
                "typed" => dec.decode::<Point>().map(|_| ()),
                "dynamic" => dec.decode::<Dynamic>().map(|_| ()),
                _ => dec.discard(),
            },
        };
        assert!(
            matches!(&result, Err(Error::Corrupt(m)) if m.contains("field numbers out of bounds")),
            "{mode}: {result:?}"
        );
    }
    // The last real field is fine.
    let mut dec = Decoder::new();
    dec.push_message(bodies[0]).unwrap();
    dec.push_message(&[0xff, 0x82, 0x02, 0x04, 0x00]).unwrap();
    assert_eq!(dec.decode::<Point>().unwrap(), Point { x: 0, y: 2 });
}

/// Types met inside a value are only recorded when the value is actually read. Go's decoder
/// never reads a value whose decode fails to compile, so a type defined inline in it stays
/// undefined for the rest of the stream.
#[test]
fn a_value_that_fails_to_decode_does_not_define_its_inline_types() {
    let stream = common::read("ifaces.gob");
    let mut dec = Decoder::new();
    for body in messages(&stream) {
        dec.push_message(body).unwrap();
    }
    // Scanned, so every inline type was seen — but only staged.
    assert!(
        dec.types().get(83).is_none(),
        "a scan must not commit inline types"
    );
    assert!(matches!(dec.decode::<i64>(), Err(Error::TypeMismatch(_))));
    assert!(dec.types().get(83).is_none());
    // A value of type 83 is now a value of an undefined type.
    assert!(matches!(
        dec.push_message(&[0xff, 0xa6, 0x00, 0x00]),
        Err(Error::UndefinedType(83))
    ));
}

/// A struct value at end of input reads no bytes, so every count-driven loop has to refuse an
/// element once the input is exhausted. Before that guard, a corrupted field count in a type
/// definition grew a test process to 116 GB.
///
/// The counts here are 2^20, not 2^60: large enough that a missing guard shows up as success
/// instead of an error, small enough that the missing guard finishes rather than spinning or
/// exhausting memory — which would turn a mutation run's verdict into an abort.
#[test]
fn huge_counts_over_zero_byte_elements_are_refused() {
    // A type definition for a struct whose field list claims 2^20 entries, then ends.
    // wireType{StructT: {CommonType{Id 65}, Field: [2^20 ...]}}
    let def = [
        0xff, 0x81, // -65
        0x03, // → StructT
        0x01, 0x02, 0xff, 0x82, 0x00, // CommonType{Id: 65}
        0x01, 0xfd, 0x10, 0x00, 0x00, // Field: count 2^20
    ];
    let r = Decoder::new().push_message(&def);
    assert!(
        matches!(&r, Err(Error::Corrupt(m)) if m.contains("length exceeds")),
        "{r:?}"
    );

    // map[Empty]Empty with a count of 2^20 and nothing behind it: key and element both read
    // zero bytes at end of input.
    #[derive(Gob, Debug, Default, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
    struct Empty {}
    #[derive(Gob, Debug, Default)]
    struct Holder {
        #[gob(name = "M")]
        m: std::collections::BTreeMap<Empty, Empty>,
    }
    let stream = Encoder::new()
        .encode(&Holder {
            m: [(Empty {}, Empty {})].into(),
        })
        .unwrap();
    let bodies = messages(&stream);
    let value = bodies.last().unwrap();
    // id, field delta 1, then the map: count 1, key terminator, element terminator, Holder's.
    assert_eq!(&value[value.len() - 4..], [0x01, 0x00, 0x00, 0x00]);
    let mut huge = value[..value.len() - 4].to_vec();
    huge.extend([0xfd, 0x10, 0x00, 0x00]);
    let mut dec = Decoder::new();
    for b in &bodies[..bodies.len() - 1] {
        dec.push_message(b).unwrap();
    }
    // The scan that every pushed value goes through refuses it. Typed and dynamic decoding carry
    // the same guard, but only ever see values this scan has already accepted, so theirs cannot
    // be reached from the public API (scripts/mutations/gobwire.plan lists them as equivalent).
    let r = dec.push_message(&huge);
    assert!(
        matches!(&r, Err(Error::Corrupt(m)) if m.contains("length exceeds")),
        "{r:?}"
    );
}
