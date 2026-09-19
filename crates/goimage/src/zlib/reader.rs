//! Port of `compress/zlib`'s reader (compress/zlib/reader.go, go1.26.4).
//!
//! `zlib.NewReader` wraps its source in a `bufio.Reader` unless it already implements
//! `flate.Reader` (`Read` + `ReadByte`). That choice is the caller's here: pass a reader with
//! `ByteRead` exactly when Go's source has `ReadByte`, and a [`BufReader`] around it otherwise.
//!
//! [`BufReader`]: crate::goread::BufReader

use crate::flate::inflate::Decompressor;
use crate::goread::{ByteRead, Error, Read, read_full};
use crate::hash::{Adler32, adler32};

/// `zlibDeflate`.
const ZLIB_DEFLATE: u8 = 8;
/// `zlibMaxWindow`.
const ZLIB_MAX_WINDOW: u8 = 7;

/// Port of zlib's `reader` (reader.go:58).
pub struct Reader<R> {
    decompressor: Decompressor<R>,
    digest: Adler32,
    err: Option<Error>,
}

/// `err == io.EOF ? io.ErrUnexpectedEOF : err`.
fn no_eof(e: Error) -> Error {
    if e == Error::Eof {
        Error::UnexpectedEof
    } else {
        e
    }
}

impl<R: ByteRead> Reader<R> {
    /// `zlib.NewReader` (reader.go:75): reads and checks the two-byte header (and a preset
    /// dictionary id, which only matches the empty dictionary) before any data.
    pub fn new(mut r: R) -> Result<Self, Error> {
        let mut scratch = [0u8; 4];
        if let (_, Some(e)) = read_full(&mut r, &mut scratch[..2]) {
            return Err(no_eof(e));
        }
        let h = u16::from_be_bytes([scratch[0], scratch[1]]);
        if scratch[0] & 0x0f != ZLIB_DEFLATE || scratch[0] >> 4 > ZLIB_MAX_WINDOW || h % 31 != 0 {
            return Err(Error::ZlibHeader);
        }
        let have_dict = scratch[1] & 0x20 != 0;
        if have_dict {
            if let (_, Some(e)) = read_full(&mut r, &mut scratch[..4]) {
                return Err(no_eof(e));
            }
            // `NewReader` passes a nil dictionary, whose Adler-32 is 1.
            if u32::from_be_bytes(scratch) != adler32(&[]) {
                return Err(Error::ZlibDictionary);
            }
        }
        Ok(Reader {
            decompressor: Decompressor::new(r, None),
            digest: Adler32::new(),
            err: None,
        })
    }

    /// The source reader, below the decompressor.
    pub fn get_mut(&mut self) -> &mut R {
        self.decompressor.get_mut()
    }
}

impl<R: ByteRead> Read for Reader<R> {
    /// `reader.Read` (reader.go:86): at the end of the deflate stream, reads and verifies the
    /// Adler-32 trailer.
    fn read(&mut self, p: &mut [u8]) -> (usize, Option<Error>) {
        if let Some(e) = &self.err {
            return (0, Some(e.clone()));
        }
        let (n, err) = self.decompressor.read(p);
        self.digest.update(&p[..n]);
        self.err = err;
        if self.err != Some(Error::Eof) {
            return (n, self.err.clone());
        }
        let mut scratch = [0u8; 4];
        if let (_, Some(e)) = read_full(self.decompressor.get_mut(), &mut scratch) {
            let e = no_eof(e);
            self.err = Some(e.clone());
            return (n, Some(e));
        }
        if u32::from_be_bytes(scratch) != self.digest.sum() {
            self.err = Some(Error::ZlibChecksum);
            return (n, Some(Error::ZlibChecksum));
        }
        (n, Some(Error::Eof))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goread::{BytesReader, read_all};

    #[test]
    fn a_preset_dictionary_id_matches_only_the_empty_dictionary() {
        // FDICT set; header 0x78 0xbb is a multiple of 31.
        let s = [0x78, 0xbb, 0, 0, 0, 1, 0x03, 0x00, 0, 0, 0, 1];
        let mut z = Reader::new(BytesReader::new(&s)).unwrap();
        assert_eq!(read_all(&mut z), (Vec::new(), None));
        let s = [0x78, 0xbb, 0, 0, 0, 2, 0x03, 0x00];
        assert_eq!(
            Reader::new(BytesReader::new(&s)).err(),
            Some(Error::ZlibDictionary)
        );
    }
}
