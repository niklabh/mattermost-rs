//! Any byte stream, decoded as dynamic values until it ends or fails: never a panic, a hang or an
//! allocation the input does not pay for.

#![no_main]

use gobwire::{Dynamic, StreamDecoder};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut decoder = StreamDecoder::new(data);
    while let Ok(Some(_)) = decoder.decode::<Dynamic>() {}
});
