//! The `io.Writer` shape the encoders write through.
//!
//! Go's encoders are chains of writers — PNG chunk writer ← `bufio.Writer` ← zlib ← flate's bit
//! writer — and where one writer's `Write` call boundaries fall is observable downstream: the PNG
//! encoder turns every `Write` it receives into one IDAT chunk. So the Rust chain keeps each
//! boundary instead of streaming into one buffer, and every stage writes through this trait.
//!
//! Writes are infallible: every sink here is in memory, as `bytes.Buffer` is in every Mattermost
//! call site, and Go's in-memory writers never fail.

/// An `io.Writer` that cannot fail.
pub trait Sink {
    /// `Write(p)`: the whole of `p`, as one call.
    fn write(&mut self, p: &[u8]);
}

impl Sink for Vec<u8> {
    fn write(&mut self, p: &[u8]) {
        self.extend_from_slice(p);
    }
}

impl<S: Sink + ?Sized> Sink for &mut S {
    fn write(&mut self, p: &[u8]) {
        (**self).write(p);
    }
}
