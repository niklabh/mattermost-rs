//! Port of `compress/zlib/writer.go`: the RFC 1950 wrapper around the deflater.

use crate::flate::Writer as FlateWriter;
use crate::flate::deflate::{BEST_COMPRESSION, FlateError, HUFFMAN_ONLY};
use crate::hash::Adler32;
use crate::sink::Sink;

/// Errors from `zlib.NewWriterLevel`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ZlibError {
    /// Go's own refusal (writer.go:65).
    #[error("zlib: invalid compression level: {0}")]
    InvalidLevel(i32),
    /// The deflater refused the level (only level 1, which is not ported).
    #[error(transparent)]
    Flate(#[from] FlateError),
}

/// Port of `zlib.Writer` without a dictionary (`NewWriterLevel`).
pub struct Writer<W: Sink> {
    level: i32,
    compressor: FlateWriter<W>,
    digest: Adler32,
    wrote_header: bool,
}

impl<W: Sink> Writer<W> {
    /// Port of `zlib.NewWriterLevel` (writer.go:52). Go creates the deflater lazily on the first
    /// write; it writes nothing on construction, so creating it here is unobservable — except that
    /// an unported level fails now rather than at the first write.
    pub fn new(w: W, level: i32) -> Result<Self, ZlibError> {
        if !(HUFFMAN_ONLY..=BEST_COMPRESSION).contains(&level) {
            return Err(ZlibError::InvalidLevel(level));
        }
        Ok(Writer {
            level,
            compressor: FlateWriter::new(w, level)?,
            digest: Adler32::new(),
            wrote_header: false,
        })
    }

    /// Port of `writeHeader` (writer.go:94).
    fn write_header(&mut self) {
        self.wrote_header = true;
        let b0: u8 = 0x78;
        let mut b1: u8 = match self.level {
            -2 | 0 | 1 => 0,
            2..=5 => 1 << 6,
            7..=9 => 3 << 6,
            _ => 2 << 6, // 6 and -1
        };
        b1 += (31 - (u16::from(b0) << 8 | u16::from(b1)) % 31) as u8;
        self.compressor.get_mut().write(&[b0, b1]);
    }

    /// `Writer.Write` (writer.go:146).
    pub fn write(&mut self, p: &[u8]) {
        if !self.wrote_header {
            self.write_header();
        }
        if p.is_empty() {
            return;
        }
        self.compressor.write(p);
        self.digest.update(p);
    }

    /// `Writer.Close` (writer.go:179): the deflate trailer, then the big-endian Adler-32.
    pub fn close(&mut self) {
        if !self.wrote_header {
            self.write_header();
        }
        self.compressor.close();
        let sum = self.digest.sum().to_be_bytes();
        self.compressor.get_mut().write(&sum);
    }

    /// The underlying writer.
    pub fn get_mut(&mut self) -> &mut W {
        self.compressor.get_mut()
    }

    /// Consume the writer, returning the underlying one. Does not close.
    pub fn into_inner(self) -> W {
        self.compressor.into_inner()
    }
}

impl<W: Sink> Sink for Writer<W> {
    fn write(&mut self, p: &[u8]) {
        Writer::write(self, p);
    }
}
