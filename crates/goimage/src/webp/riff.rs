//! Port of `golang.org/x/image/riff` (riff/riff.go) — the parts `webp/decode.go` uses.
//!
//! A RIFF stream is a sequence of chunks: an eight-byte header (a four-byte ID and a little-endian
//! uint32 length), the data, and a pad byte when the length is odd. [`Reader::next`] walks them;
//! [`Reader::chunk`] hands out the current chunk's data as a [`crate::goread::Read`].
//!
//! # What this port does not model
//!
//! Go's `chunkReader.Read` opens with two guards — a sticky error from a previous read, and
//! `errStaleReader` for a `chunkReader` used after `Next` moved on. Neither is reachable here. The
//! borrow checker ends a [`ChunkReader`]'s life before `next` can be called again, which is exactly
//! the invariant `errStaleReader` exists to catch at run time; and the underlying reader is a slice
//! cursor whose only error is `io.EOF`, which Go deliberately does not make sticky. So
//! `riff: stale reader` has no variant below: no input can produce it.

use crate::goread;

/// Every error value `riff.go` can return, rendered with Go's text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// `errMissingPaddingByte`.
    #[error("riff: missing padding byte")]
    MissingPaddingByte,
    /// `errMissingRIFFChunkHeader`.
    #[error("riff: missing RIFF chunk header")]
    MissingRiffChunkHeader,
    /// `errListSubchunkTooLong`.
    #[error("riff: list subchunk too long")]
    ListSubchunkTooLong,
    /// `errShortChunkData`.
    #[error("riff: short chunk data")]
    ShortChunkData,
    /// `errShortChunkHeader`.
    #[error("riff: short chunk header")]
    ShortChunkHeader,
    /// An error from the underlying reader, `io.EOF` included — `Next` returns it to signal that
    /// there are no more chunks.
    #[error(transparent)]
    Io(#[from] goread::Error),
}

impl Error {
    /// Whether this is Go's `io.EOF`, the "no more chunks" signal from `Next`.
    pub fn is_eof(&self) -> bool {
        *self == Error::Io(goread::Error::Eof)
    }
}

/// `riff.FourCC`.
pub type FourCC = [u8; 4];

const CHUNK_HEADER_SIZE: u32 = 8;

/// `u32` (riff.go:33): the first four bytes as a little-endian integer.
fn u32le(b: &[u8]) -> u32 {
    u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16 | u32::from(b[3]) << 24
}

/// Port of `riff.Reader`. The `bytes.Reader` Go layers underneath is inlined as `s`/`i`, so the
/// current chunk's bytes can also be peeked at without consuming them (`webp/decode.go` peeks a
/// VP8L header through a `bufio.Reader` before deciding to decode it).
pub struct Reader<'a> {
    s: &'a [u8],
    i: usize,
    err: Option<Error>,
    total_len: u32,
    chunk_len: u32,
    padded: bool,
}

/// Port of `riff.NewReader`: the stream's form type and its chunks.
pub fn new_reader(data: &[u8]) -> Result<(FourCC, Reader<'_>), Error> {
    let mut z = Reader {
        s: data,
        i: 0,
        err: None,
        total_len: 0,
        chunk_len: 0,
        padded: false,
    };
    let mut buf = [0u8; CHUNK_HEADER_SIZE as usize];
    if let Some(e) = z.read_full(&mut buf) {
        return Err(match e {
            goread::Error::Eof | goread::Error::UnexpectedEof => Error::MissingRiffChunkHeader,
            other => Error::Io(other),
        });
    }
    if &buf[0..4] != b"RIFF" {
        return Err(Error::MissingRiffChunkHeader);
    }
    let chunk_len = u32le(&buf[4..]);
    // riff.NewListReader, inlined: it is only ever called with the reader NewReader just built.
    if chunk_len < 4 {
        return Err(Error::ShortChunkData);
    }
    let mut form = [0u8; 4];
    if let Some(e) = z.read_full(&mut form) {
        return Err(match e {
            goread::Error::Eof | goread::Error::UnexpectedEof => Error::ShortChunkData,
            other => Error::Io(other),
        });
    }
    z.total_len = chunk_len - 4;
    Ok((form, z))
}

impl<'a> Reader<'a> {
    /// `bytes.Reader.Read`, inlined: `(0, io.EOF)` at the end, even for an empty `p`.
    fn read(&mut self, p: &mut [u8]) -> (usize, Option<goread::Error>) {
        if self.i >= self.s.len() {
            return (0, Some(goread::Error::Eof));
        }
        let n = p.len().min(self.s.len() - self.i);
        p[..n].copy_from_slice(&self.s[self.i..self.i + n]);
        self.i += n;
        (n, None)
    }

    /// `io.ReadFull` over [`Reader::read`], returning only the error.
    fn read_full(&mut self, buf: &mut [u8]) -> Option<goread::Error> {
        let min = buf.len();
        let mut n = 0;
        let mut err = None;
        while n < min && err.is_none() {
            let (nn, e) = self.read(&mut buf[n..]);
            n += nn;
            err = e;
        }
        if n >= min {
            None
        } else if n > 0 && err == Some(goread::Error::Eof) {
            Some(goread::Error::UnexpectedEof)
        } else {
            err
        }
    }

    /// Port of `Reader.Next`: the next chunk's ID and length. `Err(e)` with [`Error::is_eof`]
    /// means there are no more chunks. Calling it is valid even when the previous chunk was not
    /// read out.
    //
    // Named for the Go method it ports, not for `Iterator::next`.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<(FourCC, u32), Error> {
        if let Some(e) = &self.err {
            return Err(e.clone());
        }

        // Drain the rest of the previous chunk. Go runs `io.Copy(io.Discard, z.chunkReader)`, which
        // stops at the chunkReader's io.EOF and reports how much it moved.
        if self.chunk_len != 0 {
            let want = self.chunk_len;
            let mut got: u64 = 0;
            let mut scratch = [0u8; 8192];
            loop {
                let (n, err) = self.chunk_read(&mut scratch);
                got += n as u64;
                match err {
                    None => {}
                    Some(goread::Error::Eof) => break,
                    Some(e) => return Err(self.fail(Error::Io(e))),
                }
            }
            if got as u32 != want {
                return Err(self.fail(Error::ShortChunkData));
            }
        }
        if self.padded {
            if self.total_len == 0 {
                return Err(self.fail(Error::ListSubchunkTooLong));
            }
            self.total_len -= 1;
            let mut pad = [0u8; 1];
            if let Some(e) = self.read_full(&mut pad) {
                let e = if e == goread::Error::Eof {
                    Error::MissingPaddingByte
                } else {
                    Error::Io(e)
                };
                return Err(self.fail(e));
            }
        }

        // We are done if we have no more data.
        if self.total_len == 0 {
            return Err(self.fail(Error::Io(goread::Error::Eof)));
        }

        // Read the next chunk header.
        if self.total_len < CHUNK_HEADER_SIZE {
            return Err(self.fail(Error::ShortChunkHeader));
        }
        self.total_len -= CHUNK_HEADER_SIZE;
        let mut buf = [0u8; CHUNK_HEADER_SIZE as usize];
        if let Some(e) = self.read_full(&mut buf) {
            let e = match e {
                goread::Error::Eof | goread::Error::UnexpectedEof => Error::ShortChunkHeader,
                other => Error::Io(other),
            };
            return Err(self.fail(e));
        }
        let chunk_id = [buf[0], buf[1], buf[2], buf[3]];
        self.chunk_len = u32le(&buf[4..]);
        if self.chunk_len > self.total_len {
            return Err(self.fail(Error::ListSubchunkTooLong));
        }
        self.padded = self.chunk_len & 1 == 1;
        Ok((chunk_id, self.chunk_len))
    }

    /// Record the sticky error Go keeps in `z.err` and hand it back.
    fn fail(&mut self, e: Error) -> Error {
        self.err = Some(e.clone());
        e
    }

    /// Port of `chunkReader.Read`. See the module comment for the two guards Go opens with and why
    /// neither is reachable here.
    fn chunk_read(&mut self, p: &mut [u8]) -> (usize, Option<goread::Error>) {
        let n = self.chunk_len as usize;
        if n == 0 {
            return (0, Some(goread::Error::Eof));
        }
        // Go also clamps a uint32 that overflowed a 32-bit `int` to MaxInt32; on a 64-bit target
        // the conversion is always exact.
        let n = n.min(p.len());
        let (n, err) = self.read(&mut p[..n]);
        self.total_len = self.total_len.wrapping_sub(n as u32);
        self.chunk_len = self.chunk_len.wrapping_sub(n as u32);
        if err != Some(goread::Error::Eof) {
            self.err = err.clone().map(Error::Io);
        }
        (n, err)
    }

    /// The current chunk's data as a reader — Go's `chunkData`.
    pub fn chunk(&mut self) -> ChunkReader<'_, 'a> {
        ChunkReader { z: self }
    }

    /// The next `n` bytes of the current chunk without consuming them, short when the chunk or the
    /// input ends first. This is what `bufio.NewReader(chunkData).Peek(n)` returns, given that
    /// bufio's buffer (4096 bytes) is larger than every `n` the WebP decoder peeks.
    pub fn peek_chunk(&self, n: usize) -> &'a [u8] {
        let s: &'a [u8] = self.s;
        let avail = (s.len() - self.i).min(self.chunk_len as usize);
        &s[self.i..self.i + avail.min(n)]
    }
}

/// The current chunk's data. Go's `*riff.chunkReader`.
pub struct ChunkReader<'r, 'a> {
    z: &'r mut Reader<'a>,
}

impl goread::Read for ChunkReader<'_, '_> {
    fn read(&mut self, p: &mut [u8]) -> (usize, Option<goread::Error>) {
        self.z.chunk_read(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goread::Read as _;

    fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut b = id.to_vec();
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(data);
        if data.len() & 1 == 1 {
            b.push(0);
        }
        b
    }

    fn file(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.concat();
        let mut b = b"RIFF".to_vec();
        b.extend_from_slice(&((4 + body.len()) as u32).to_le_bytes());
        b.extend_from_slice(b"WEBP");
        b.extend_from_slice(&body);
        b
    }

    #[test]
    fn a_short_header_is_a_missing_header() {
        for n in 0..8 {
            assert_eq!(
                new_reader(&b"RIFF\x04\x00\x00\x00"[..n]).err(),
                Some(Error::MissingRiffChunkHeader),
                "{n}"
            );
        }
        assert_eq!(
            new_reader(b"XIFF\x04\x00\x00\x00WEBP").err(),
            Some(Error::MissingRiffChunkHeader)
        );
    }

    #[test]
    fn a_list_shorter_than_its_form_type_is_short_chunk_data() {
        for len in 0..4u32 {
            let mut b = b"RIFF".to_vec();
            b.extend_from_slice(&len.to_le_bytes());
            b.extend_from_slice(b"WEBP");
            assert_eq!(new_reader(&b).err(), Some(Error::ShortChunkData), "{len}");
        }
        assert_eq!(
            new_reader(b"RIFF\x04\x00\x00\x00WE").err(),
            Some(Error::ShortChunkData)
        );
    }

    #[test]
    fn next_walks_chunks_and_ends_with_eof() {
        let data = file(&[chunk(b"ONE ", b"abcd"), chunk(b"TWO ", b"xyz")]);
        let (form, mut z) = new_reader(&data).unwrap();
        assert_eq!(&form, b"WEBP");
        assert_eq!(z.next().unwrap(), (*b"ONE ", 4));
        let mut p = [0u8; 8];
        assert_eq!(z.chunk().read(&mut p), (4, None));
        assert_eq!(&p[..4], b"abcd");
        assert_eq!(z.chunk().read(&mut p), (0, Some(goread::Error::Eof)));
        // The odd-length chunk's pad byte is consumed by the following Next.
        assert_eq!(z.next().unwrap(), (*b"TWO ", 3));
        assert!(z.next().unwrap_err().is_eof());
        // The error is sticky.
        assert!(z.next().unwrap_err().is_eof());
    }

    #[test]
    fn an_undrained_chunk_is_drained_by_the_next_call() {
        let data = file(&[chunk(b"ONE ", &[7u8; 5000]), chunk(b"TWO ", b"ab")]);
        let (_, mut z) = new_reader(&data).unwrap();
        assert_eq!(z.next().unwrap(), (*b"ONE ", 5000));
        let mut p = [0u8; 3];
        assert_eq!(z.chunk().read(&mut p), (3, None));
        assert_eq!(z.next().unwrap(), (*b"TWO ", 2));
    }

    #[test]
    fn a_chunk_shorter_than_it_claims_is_short_chunk_data() {
        let mut data = b"RIFF".to_vec();
        data.extend_from_slice(&(4u32 + 8 + 100).to_le_bytes());
        data.extend_from_slice(b"WEBPJUNK");
        data.extend_from_slice(&100u32.to_le_bytes());
        data.extend_from_slice(b"0123456789");
        let (_, mut z) = new_reader(&data).unwrap();
        assert_eq!(z.next().unwrap(), (*b"JUNK", 100));
        assert_eq!(z.next().err(), Some(Error::ShortChunkData));
    }

    #[test]
    fn peek_stops_at_the_chunk_end() {
        let data = file(&[chunk(b"ONE ", b"abc"), chunk(b"TWO ", b"wxyz")]);
        let (_, mut z) = new_reader(&data).unwrap();
        z.next().unwrap();
        assert_eq!(z.peek_chunk(5), b"abc");
        assert_eq!(z.peek_chunk(2), b"ab");
        // Peeking does not consume.
        let mut p = [0u8; 3];
        assert_eq!(z.chunk().read(&mut p), (3, None));
        assert_eq!(&p, b"abc");
    }
}
