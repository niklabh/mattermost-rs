//! Reading gob messages from an async byte stream.

use gobwire::{Decoder, Progress};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::Error;

/// Read one message body. `Ok(None)` at a clean end of input before any byte of it.
async fn read_message<R: AsyncRead + Unpin>(r: &mut R, body: &mut Vec<u8>) -> Result<bool, Error> {
    let mut first = [0u8; 1];
    if r.read(&mut first).await? == 0 {
        return Ok(false);
    }
    let len = if first[0] < 0x80 {
        u64::from(first[0])
    } else {
        let width = usize::from(first[0].wrapping_neg());
        if width > 8 {
            return Err(gobwire::Error::BadUint.into());
        }
        let mut b = [0u8; 8];
        r.read_exact(&mut b[..width]).await.map_err(eof)?;
        b[..width]
            .iter()
            .fold(0u64, |acc, &x| acc << 8 | u64::from(x))
    };
    if len >= gobwire::MAX_MESSAGE_LEN {
        return Err(gobwire::Error::Corrupt("invalid message length".into()).into());
    }
    let len = usize::try_from(len)
        .map_err(|_| gobwire::Error::Corrupt("invalid message length".into()))?;
    body.clear();
    // Grow as bytes arrive rather than trusting the length with an allocation up front.
    let got = r.take(len as u64).read_to_end(body).await?;
    if got != len {
        return Err(Error::UnexpectedEof);
    }
    Ok(true)
}

fn eof(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        Error::UnexpectedEof
    } else {
        e.into()
    }
}

/// Read messages until `dec` holds a complete value. `Ok(false)` at a clean end of input before
/// the value began (Go's `io.EOF`); an end after a type definition or mid-value is
/// [`Error::UnexpectedEof`].
pub(crate) async fn read_value<R: AsyncRead + Unpin>(
    r: &mut R,
    dec: &mut Decoder,
    body: &mut Vec<u8>,
) -> Result<bool, Error> {
    let mut started = false;
    loop {
        if !read_message(r, body).await? {
            return if started {
                Err(Error::UnexpectedEof)
            } else {
                Ok(false)
            };
        }
        started = true;
        if dec.push_message(body)? == Progress::Ready {
            return Ok(true);
        }
    }
}
