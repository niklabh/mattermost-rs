//! Blocking stream helpers. Async transports frame messages with
//! [`parse_length_prefix`](crate::parse_length_prefix) and feed [`Decoder`] directly.

use std::io::{self, Read, Write};

use crate::decode::{Decode, Decoder, Progress};
use crate::encode::{Encode, Encoder};
use crate::error::{Error, Result};
use crate::wire::MAX_MESSAGE_LEN;

/// Read one length-prefixed message body. `Ok(None)` at a clean end of input.
pub fn read_message<R: Read>(r: &mut R) -> Result<Option<Vec<u8>>> {
    let mut first = [0u8; 1];
    match r.read_exact(&mut first) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let n = if first[0] < 0x80 {
        u64::from(first[0])
    } else {
        let width = usize::from(first[0].wrapping_neg());
        if width > 8 {
            return Err(Error::BadUint);
        }
        let mut b = [0u8; 8];
        r.read_exact(&mut b[..width]).map_err(eof)?;
        b[..width]
            .iter()
            .fold(0u64, |acc, &x| acc << 8 | u64::from(x))
    };
    if n >= MAX_MESSAGE_LEN {
        return Err(Error::Corrupt("invalid message length".into()));
    }
    let n = usize::try_from(n).map_err(|_| Error::Corrupt("invalid message length".into()))?;
    let mut body = Vec::new();
    r.take(n as u64).read_to_end(&mut body)?;
    if body.len() != n {
        return Err(Error::UnexpectedEof);
    }
    Ok(Some(body))
}

fn eof(e: io::Error) -> Error {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        Error::UnexpectedEof
    } else {
        e.into()
    }
}

/// A [`Decoder`] reading from a blocking source (Go's `gob.NewDecoder(r)`).
pub struct StreamDecoder<R> {
    reader: R,
    decoder: Decoder,
}

impl<R: Read> StreamDecoder<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            decoder: Decoder::new(),
        }
    }

    /// Read messages until a value is complete. `Ok(false)` at a clean end of input before any
    /// message (Go's `io.EOF`); an end after a type definition or mid-value is an error (Go's
    /// `io.ErrUnexpectedEOF`).
    fn fill(&mut self) -> Result<bool> {
        let mut started = false;
        loop {
            let Some(body) = read_message(&mut self.reader)? else {
                return if started {
                    Err(Error::UnexpectedEof)
                } else {
                    Ok(false)
                };
            };
            started = true;
            if self.decoder.push_message(&body)? == Progress::Ready {
                return Ok(true);
            }
        }
    }

    /// Decode the next value into `dest`. `Ok(false)` at end of input.
    pub fn decode_into<T: Decode + ?Sized>(&mut self, dest: &mut T) -> Result<bool> {
        if !self.fill()? {
            return Ok(false);
        }
        self.decoder.decode_into(dest)?;
        Ok(true)
    }

    /// Decode the next value. `Ok(None)` at end of input.
    pub fn decode<T: Decode + Default>(&mut self) -> Result<Option<T>> {
        let mut v = T::default();
        Ok(if self.decode_into(&mut v)? {
            Some(v)
        } else {
            None
        })
    }

    pub fn into_inner(self) -> R {
        self.reader
    }
}

/// An [`Encoder`] writing to a blocking sink (Go's `gob.NewEncoder(w)`).
pub struct StreamEncoder<W> {
    writer: W,
    encoder: Encoder,
    buf: Vec<u8>,
}

impl<W: Write> StreamEncoder<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            encoder: Encoder::new(),
            buf: Vec::new(),
        }
    }

    pub fn encode<T: Encode + ?Sized>(&mut self, value: &T) -> Result<()> {
        self.buf.clear();
        self.encoder.encode_into(value, &mut self.buf)?;
        self.writer.write_all(&self.buf)?;
        Ok(())
    }

    pub fn into_inner(self) -> W {
        self.writer
    }
}
