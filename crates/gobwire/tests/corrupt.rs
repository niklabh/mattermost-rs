//! Malformed input never panics: every Go-written stream in the corpus, corrupted byte by byte
//! and truncated at every length, either decodes or returns an error.
//!
//! Not a substitute for coverage-guided fuzzing (`fuzz/`, run by `scripts/crates-preflight.sh
//! --fuzz`), but it walks every parser branch the corpus reaches with values it did not expect,
//! on every `cargo test`.

mod common;

use common::{cases, messages, read};
use gobwire::{Decoder, Dynamic, Progress};

/// Push every message, then decode dynamically and by discarding; the result is ignored, only a
/// panic fails the test.
fn exercise(bodies: &[Vec<u8>]) {
    for mode in 0..2 {
        let mut dec = Decoder::new();
        for body in bodies {
            match dec.push_message(body) {
                Ok(Progress::Ready) => {
                    let _ = if mode == 0 {
                        dec.decode::<Dynamic>().map(|_| ())
                    } else {
                        dec.discard()
                    };
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
}

#[test]
fn corrupted_and_truncated_streams_never_panic() {
    let mut runs = 0usize;
    for case in cases() {
        let stream = read(&case.stream);
        let bodies: Vec<Vec<u8>> = messages(&stream).into_iter().map(<[u8]>::to_vec).collect();
        for m in 0..bodies.len() {
            let len = bodies[m].len();
            // Large messages: every 7th byte keeps the sweep quick in a debug build.
            let stride = if len > 256 { 7 } else { 1 };
            for pos in (0..len).step_by(stride) {
                for replacement in [0x00, 0x01, 0x7f, 0x80, 0xf8, 0xff, bodies[m][pos] ^ 0x55] {
                    let mut corrupt = bodies.clone();
                    corrupt[m][pos] = replacement;
                    exercise(&corrupt);
                    runs += 1;
                }
            }
            for cut in 0..len {
                let mut truncated = bodies.clone();
                truncated[m].truncate(cut);
                exercise(&truncated);
                runs += 1;
            }
        }
    }
    assert!(runs > 10_000, "only {runs} corrupted streams exercised");
}
