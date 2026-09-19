//! Whatever decodes must re-encode to a stream that decodes, and encoding is stable after one
//! pass: encode(decode(encode(v))) produces the bytes encode(v) did.

#![no_main]

use gobwire::{Dynamic, Encoder, StreamDecoder};
use libfuzzer_sys::fuzz_target;

fn decode_one(bytes: &[u8]) -> Dynamic {
    match StreamDecoder::new(bytes).decode::<Dynamic>() {
        Ok(Some(v)) => v,
        other => panic!("gobwire cannot decode its own encoding: {other:?}"),
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(Some(value)) = StreamDecoder::new(data).decode::<Dynamic>() else {
        return;
    };
    let Ok(first) = Encoder::new().encode(&value) else {
        return;
    };
    let again = decode_one(&first);
    let second = Encoder::new()
        .encode(&again)
        .expect("a value decoded from gobwire's own encoding re-encodes");
    assert_eq!(first, second, "re-encoding is not stable");
});
