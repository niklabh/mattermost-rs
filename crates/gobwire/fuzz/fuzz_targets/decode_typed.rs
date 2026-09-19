//! Any byte stream, decoded into a struct that covers every kind of field `Decode` implements:
//! the path a program takes when it knows the type it expects, with field matching, merging
//! and type checking against the stream's definitions.

#![no_main]

use std::collections::{BTreeMap, HashMap};

use gobwire::{Complex, GoTime, Gob, Interface, StreamDecoder};
use libfuzzer_sys::fuzz_target;

#[derive(Gob, Debug, Default)]
struct Inner {
    #[gob(name = "Name")]
    name: String,
    #[gob(name = "Next")]
    next: Option<Box<Inner>>,
}

#[derive(Gob, Debug, Default)]
struct Everything {
    #[gob(name = "B")]
    b: bool,
    #[gob(name = "I")]
    i: i64,
    #[gob(name = "I8")]
    i8: i8,
    #[gob(name = "U")]
    u: u64,
    #[gob(name = "U16")]
    u16: u16,
    #[gob(name = "F")]
    f: f64,
    #[gob(name = "F32")]
    f32: f32,
    #[gob(name = "C")]
    c: Complex,
    #[gob(name = "S")]
    s: String,
    #[gob(name = "Bytes")]
    bytes: Vec<u8>,
    #[gob(name = "Arr")]
    arr: [i32; 3],
    #[gob(name = "List")]
    list: Vec<Inner>,
    #[gob(name = "Map")]
    map: HashMap<String, Vec<i64>>,
    #[gob(name = "Sorted")]
    sorted: BTreeMap<i64, String>,
    #[gob(name = "Ptr")]
    ptr: Option<Box<Inner>>,
    #[gob(name = "Any")]
    any: Option<Interface>,
    #[gob(name = "When")]
    when: GoTime,
}

fuzz_target!(|data: &[u8]| {
    let mut decoder = StreamDecoder::new(data);
    // Decoding merges, so one destination across the stream exercises reuse as Go does.
    let mut into = Everything::default();
    while let Ok(true) = decoder.decode_into(&mut into) {}
});
